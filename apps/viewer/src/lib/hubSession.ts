import { encodeBase64Url } from "./hub";
import type { NoiseChannel } from "./hubCrypto";
import { disconnected, type RecordTransport } from "./hubTransport";

export type HubFrame = { type: string; [field: string]: unknown };
export const HUB_CHUNK_BYTES = 32 * 1024;
const RESPONSE_WINDOW = 8;
export const MAX_HUB_STREAMS = 16;
const CONTROL_RESPONSES: Readonly<Record<string, string | undefined>> = {
  discovery_request: "discovery_response",
  direct_config_request: "direct_config",
  direct_offer: "direct_answer",
};
const MAX_STREAM_ID = 0xffffffff;
const timedOut = () => new Error("Viewer request timed out; delivery may be uncertain.");

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error("Encrypted connection failed; delivery may be uncertain.");
}

/** An authenticated channel owns both Noise counters. Requests only see their own frames. */
export class HubSession {
  received = 0;
  private streams = new Map<number, HubStream>();
  private lastId = 0;
  private writing: Promise<void> = Promise.resolve();
  private failure?: Error;
  private retiring = false;

  constructor(private socket: RecordTransport, private channel: NoiseChannel, private onClose: (error: Error) => void) {
    // Each stream can have a header, its entire credited window, and an end
    // ready in the same carrier burst. The transport also caps queued bytes.
    socket.setReceiveWindow(MAX_HUB_STREAMS * (RESPONSE_WINDOW + 2));
    void this.receive();
  }

  open(header: HubFrame, body: Uint8Array, deadline?: number): HubStream {
    if (this.failure || this.retiring) throw this.failure ?? disconnected();
    if (this.streams.size >= MAX_HUB_STREAMS) throw new Error("Too many active viewer requests.");
    if (this.lastId === MAX_STREAM_ID) {
      const error = new Error("Machine connection is renewing.");
      this.close(error);
      throw error;
    }
    const id = ++this.lastId;
    const terminalReply = CONTROL_RESPONSES[header.type];
    const live = header.path === "/api/v1/events";
    const stream = new HubStream(this, id, live, terminalReply, deadline);
    this.streams.set(id, stream);
    // Queue the first frame in allocation order. Each later chunk yields the
    // writer to other streams, including window credit for live updates.
    const upload = async () => {
      await this.write(id, header);
      if (terminalReply) return;
      for (let offset = 0; offset < body.length; offset += HUB_CHUNK_BYTES) {
        await this.write(id, { type: "request_body", data: encodeBase64Url(body.slice(offset, offset + HUB_CHUNK_BYTES).buffer) });
      }
      await this.write(id, { type: "request_end" });
    };
    void upload().catch((error) => stream.close(asError(error)));
    return stream;
  }

  write(id: number, frame: HubFrame, sent?: () => void): Promise<void> {
    // The writer owns encryption and send together. A failed write may have
    // advanced the Noise nonce and must terminate the physical channel.
    const writing = this.writing.then(async () => {
      if (this.failure) throw this.failure;
      await this.socket.waitWritable();
      if (this.failure) throw this.failure;
      const stream = this.streams.get(id);
      if (!stream && frame.type !== "cancel") throw disconnected();
      try {
        this.socket.send(this.channel.encrypt_json(JSON.stringify({ type: "stream", stream_id: id, frame })));
        if (stream) stream.sent = true;
        sent?.();
      } catch (error) {
        this.close(asError(error));
        throw error;
      }
    });
    this.writing = writing.catch(() => {});
    return writing;
  }

  private async receive(): Promise<void> {
    try {
      while (!this.failure) {
        const record = await this.socket.read(0);
        if (this.failure) return;
        const message = JSON.parse(this.channel.decrypt_json(record)) as HubFrame;
        if (message.type === "error") throw new Error(String(message.message));
        const id = message.stream_id;
        if (message.type !== "stream" || typeof id !== "number" || !Number.isInteger(id) || id < 1 || id > this.lastId
          || !message.frame || typeof message.frame !== "object") throw new Error("Machine returned an invalid stream record.");
        this.received++;
        // In-flight responses to a locally canceled stream can arrive until
        // the host sees its cancel. IDs are never reused on this channel.
        this.streams.get(id)?.deliver(message.frame as HubFrame);
      }
    } catch (error) { this.close(asError(error)); }
  }

  release(stream: HubStream, cancel: boolean): void {
    if (!this.streams.delete(stream.id)) return;
    if (cancel && stream.sent && !this.failure) void this.write(stream.id, { type: "cancel" }).catch(() => {});
    if (this.retiring && !this.streams.size) this.close();
  }

  retire(): void {
    this.retiring = true;
    if (!this.streams.size) this.close();
  }

  close(error = disconnected()): void {
    if (this.failure) return;
    this.failure = error;
    this.socket.close(error);
    this.channel.free();
    for (const stream of this.streams.values()) stream.close(error);
    this.onClose(error);
  }
}

export class HubStream {
  sent = false;
  private responseStarted = false;
  private terminal = false;
  private failure?: Error;
  private outstanding = 0;
  private buffered: HubFrame[] = [];
  private wake?: () => void;
  private deadlineTimer?: ReturnType<typeof setTimeout>;

  constructor(private session: HubSession, readonly id: number, private live: boolean, private terminalReply?: string, deadline?: number) {
    if (deadline !== undefined) this.deadlineTimer = setTimeout(() => this.close(timedOut()), deadline - Date.now());
  }

  deliver(frame: HubFrame): void {
    if (this.terminal) throw new Error("Machine returned data after ending a stream.");
    if (frame.type === "error") this.terminal = true;
    else if (this.terminalReply && frame.type === this.terminalReply) this.terminal = true;
    else if (!this.terminalReply && !this.responseStarted && frame.type === "response") this.responseStarted = true;
    else if (this.responseStarted && frame.type === "chunk") {
      if (++this.outstanding > RESPONSE_WINDOW) throw new Error("Machine exceeded the encrypted response window.");
    } else if (this.responseStarted && frame.type === "end") this.terminal = true;
    else throw new Error("Machine returned an unexpected encrypted response.");
    this.buffered.push(frame);
    this.wake?.();
  }

  async next(timeout?: number): Promise<HubFrame> {
    if (!this.buffered.length && !this.failure) {
      if (this.terminal || this.wake) throw new Error("Invalid encrypted stream read.");
      let timer: ReturnType<typeof setTimeout> | undefined;
      if (timeout !== undefined) {
        const received = this.session.received;
        timer = setTimeout(() => {
          // An established event stream receives periodic authenticated data.
          // Replace a silent carrier, or cancel just this stream when another
          // response has demonstrated that the channel is still working.
          if (this.live && this.session.received === received) this.session.close(new Error("Machine stopped sending live updates."));
          else this.close(timedOut());
        }, timeout);
      }
      try { await new Promise<void>((resolve) => { this.wake = resolve; }); }
      finally { clearTimeout(timer); this.wake = undefined; }
    }
    if (this.failure) throw this.failure;
    const message = this.buffered.shift()!;
    if (message.type === "error") throw new Error(String(message.message));
    return message;
  }

  async acknowledge(): Promise<void> {
    if (this.failure) throw this.failure;
    if (this.outstanding <= 0) throw new Error("No response chunk awaits credit.");
    if (this.terminal) this.outstanding--;
    else await this.session.write(this.id, { type: "window", credits: 1 }, () => { this.outstanding--; });
  }

  close(error = disconnected()): void {
    if (this.failure) return;
    this.failure = error;
    clearTimeout(this.deadlineTimer);
    this.buffered = [];
    this.wake?.();
    this.session.release(this, !this.terminal);
  }
}
