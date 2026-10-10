import { afterEach, expect, it, vi } from "vitest";
import { MAX_RECORD, WebRtcPeer } from "./hubTransport";

class Channel extends EventTarget {
  label = "tokn-record-v1";
  binaryType = "";
  bufferedAmount = 0;
  readyState: RTCDataChannelState = "open";
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  onclose: ((event: Event) => void) | null = null;
  sent: Uint8Array[] = [];
  send(record: Uint8Array) { this.sent.push(record); }
  close() { this.readyState = "closed"; this.onclose?.(new Event("close")); this.dispatchEvent(new Event("close")); }
  push(record: Uint8Array) { this.onmessage?.(new MessageEvent("message", { data: Uint8Array.from(record).buffer })); }
}
class Peer extends EventTarget {
  iceGatheringState: RTCIceGatheringState = "complete";
  connectionState: RTCPeerConnectionState = "connected";
  localDescription?: RTCSessionDescriptionInit;
  onconnectionstatechange: (() => void) | null = null;
  sctp = { maxMessageSize: MAX_RECORD };
  channels: Channel[] = [];
  createDataChannel = vi.fn((_label: string, _options: RTCDataChannelInit) => { const channel = new Channel(); this.channels.push(channel); return channel; });
  createOffer = vi.fn().mockResolvedValue({ type: "offer", sdp: "v=0\r\n" });
  async setLocalDescription(description: RTCSessionDescriptionInit) { this.localDescription = description; }
  setRemoteDescription = vi.fn().mockResolvedValue(undefined);
  close = vi.fn();
}
function setup() {
  const raw = new Peer();
  const config = vi.fn(() => raw as unknown as RTCPeerConnection);
  const controller = new AbortController();
  const failed = vi.fn();
  const peer = new WebRtcPeer(["stun:stun.example:3478"], controller.signal, failed, config);
  return { peer, raw, controller, failed, config };
}
afterEach(() => vi.useRealTimers());

it("uses one peer and fresh ordered data channels with the shared record bound", async () => {
  const { peer, raw, config } = setup();
  expect(await peer.offer()).toBe("v=0\r\n"); await peer.accept("answer");
  const first = await peer.open(new AbortController().signal);
  const second = await peer.open(new AbortController().signal);
  expect(raw.channels).toHaveLength(2);
  expect(raw.createDataChannel).toHaveBeenCalledWith("tokn-record-v1", { ordered: true });
  expect(config).toHaveBeenCalledWith({ iceServers: [{ urls: "stun:stun.example:3478" }] });
  first.send(new Uint8Array([1, 2])); raw.channels[0].push(new Uint8Array([3, 4]));
  expect(await first.read()).toEqual(new Uint8Array([3, 4]));
  expect(raw.channels[1].sent).toEqual([]);
  first.close(); expect(raw.channels[1].readyState).toBe("open");
  second.close(); peer.close(); expect(raw.close).toHaveBeenCalledOnce();
});

it("rejects narrow SCTP channels and excessive outgoing buffers", async () => {
  const narrow = setup(); narrow.raw.sctp.maxMessageSize = 16_384;
  await expect(narrow.peer.open(new AbortController().signal)).rejects.toThrow("record size"); narrow.peer.close();
  const { peer, raw } = setup();
  const records = await peer.open(new AbortController().signal);
  raw.channels[0].bufferedAmount = 2 * 1024 * 1024;
  expect(() => records.send(new Uint8Array([1]))).toThrow("buffer limit"); peer.close();
});

it("bounds inbound queues and cancels pending readers when their exchange closes", async () => {
  const { peer, raw } = setup();
  const controller = new AbortController();
  const records = await peer.open(controller.signal);
  const result = records.read(); const rejected = expect(result).rejects.toThrow("Machine disconnected");
  controller.abort(); await rejected;
  const next = await peer.open(new AbortController().signal);
  for (let index = 0; index < 17; index++) raw.channels[1].push(new Uint8Array([index]));
  await expect(next.read()).rejects.toThrow("queue exceeds"); peer.close();
});

it("aborts unfinished negotiation and disposes the entire peer on machine cancellation", async () => {
  const { peer, raw, controller } = setup(); raw.iceGatheringState = "gathering";
  const offer = peer.offer(); const rejected = expect(offer).rejects.toThrow("Machine disconnected");
  await vi.waitFor(() => expect(raw.localDescription).toBeDefined()); controller.abort(); await rejected;
  expect(raw.close).toHaveBeenCalledOnce(); expect(raw.channels[0].readyState).toBe("closed");
});

it("reports a direct peer failure once and closes every exchange", () => {
  const { peer, raw, failed } = setup();
  raw.connectionState = "failed"; raw.onconnectionstatechange?.(); raw.onconnectionstatechange?.();
  expect(failed).toHaveBeenCalledOnce(); expect(raw.channels[0].readyState).toBe("closed"); peer.close();
});

it("renews the peer before retained SCTP channel bookkeeping grows without a bound", async () => {
  const { peer, failed, raw } = setup();
  const active = await peer.open(new AbortController().signal);
  for (let index = 1; index < 512; index++) (await peer.open(new AbortController().signal)).close();
  await expect(peer.open(new AbortController().signal)).rejects.toThrow("renewing");
  expect(failed).toHaveBeenCalledOnce(); expect(raw.close).not.toHaveBeenCalled();
  active.send(new Uint8Array([1])); expect(raw.channels[0].readyState).toBe("open");
  active.close(); expect(raw.close).toHaveBeenCalledOnce();
});

it("bounds graceful retirement and machine cancellation still closes retiring channels immediately", async () => {
  vi.useFakeTimers();
  const deadline = setup();
  await deadline.peer.open(new AbortController().signal); deadline.peer.retire();
  await vi.advanceTimersByTimeAsync(119_999); expect(deadline.raw.close).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1); expect(deadline.raw.close).toHaveBeenCalledOnce();
  const canceled = setup();
  await canceled.peer.open(new AbortController().signal); canceled.peer.retire(); canceled.controller.abort();
  expect(canceled.raw.close).toHaveBeenCalledOnce(); expect(canceled.raw.channels[0].readyState).toBe("closed");
});
