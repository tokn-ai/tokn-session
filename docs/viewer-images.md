# Viewer images

The viewer displays recorded image messages from Codex, Pi, OpenCode, and ZCode.
Image-only messages remain visible. Message cards show lazy thumbnails and open
a larger preview; missing files, unsupported formats, relative paths, remote URLs,
and opaque provider references retain an unavailable/error placeholder. The host
never fetches a recorded remote URL. Image normalization records references without
reading their files.

The session composer and local **New Pi Session** dialog accept file picking,
clipboard images, and drag/drop, with removable previews and image-only submission.
Existing Codex Desktop and Pi input transports handle sending. Codex uses validated
host-local image paths; Pi uses native image content with base64 bytes and MIME
type. Pi advertises image support from the current model and rechecks it before
sending; old bridges without the capability field remain text-only. New managed
Pi sessions check the actual model after startup. Unsupported images produce an
explicit failure rather than silently sending only their caption.

Stopped viewer-owned Pi sessions expose no image capability until Resume verifies
the current model; status inspection never starts a worker. Image-bearing requests
preserve their attachments through automatic resume and recheck model support
before sending.

## Limits and resources

PNG, JPEG, WebP, and GIF are supported. Input accepts at most four images, 8 MiB
per image and 24 MiB total. Host validation checks the actual format and decoding,
including a 33,554,432-pixel limit, a 16,384-pixel limit per dimension, and a bounded
160 MiB decoder allocation. Recorded-image reads use the same raster/file limits;
message summaries expose at most 64 image references. Thumbnails fit within
480 × 360 pixels and use PNG; full previews retain the original supported bytes.

Image bytes are separate resources, independent of timeline delivery levels:

- `load_session_image` takes `session_key`, `event_key`, `image_id`, and
  `variant` (`thumbnail` or `full`). The host resolves the image from that exact
  visible message, checks session access, and returns MIME type, base64 data,
  width, and height. Clients cannot supply a path or URL to read.
- `upload_image_chunk` takes a session/project target, UUID `upload_id`, byte
  `offset`, declared total size/MIME/name, and base64 data. Chunks contain at most
  256 KiB of decoded bytes. Offsets must be contiguous; an identical already
  received chunk may be retried. Completion validates the raster and returns an
  opaque `image_id` used in the separate message/create request.
- `release_image_upload` disposes unused uploads for their original target.
  It cannot remove assets retained for possible provider delivery.

These commands share the Tauri, HTTP, and encrypted Hub adapters. HTTP uses the
existing `/api/v1/<command>` request envelope. The small upload chunks fit existing
Hub request limits. Session timelines and display details carry compact image
metadata, without retransmitting image bytes or exposing source paths. The
frontend has a separate per-machine blob cache bounded to 64 resources and
40 MiB, with de-duplicated reads, pinned visible resources, and URL cleanup on
transport closure. Its keys include the message content revision and variant.

## Storage and delivery

Native uploads live in `~/.tokn/sessions/viewer/images`. With an explicit index
path they live under `viewer/images` beside that index. Unix storage uses a
private directory and files. Upload IDs are bound to one session or registered
project; completed uploads are immutable and SHA-256 is checked again at delivery.
At most 64 upload records and 128 MiB of unretained staging are admitted per host
process. Unused uploads expire after 30 minutes of inactivity; later upload work
and startup recovery remove expired, unretained files.

Before potential Codex delivery, assets receive a private retention marker and
keep their original host paths. This preserves Desktop references and uncertain
delivery recovery, including conservative retention after definite rejections.
Retained Codex files are excluded from upload expiry/removal; automatic retained
asset garbage collection is not implemented. Pi and managed Pi receive embedded
bytes owned by detached delivery work, so their staged files remain eligible for
normal expiry/release and are not permanently retained. Upload identifiers are
process-local, so an unsent draft must upload again after a host restart.

Message receipts include ordered image identities; managed Pi and its bridge also
bind image content hashes. Lost acknowledgments remain uncertain and are never
automatically resent. Definite upload/model failures leave editable drafts, while
uncertain delivery requires the existing explicit review/edit recovery.

## Permissions

Image reads require access to their exact session/event. Read-only session shares
may display images in their allowed sessions, but cannot upload or send. Hub
uploads and release commands require host control permission. Project uploads,
project/session creation, and managed Pi controls remain local-desktop features;
browser/Hub clients can attach images only to supported existing sessions.
