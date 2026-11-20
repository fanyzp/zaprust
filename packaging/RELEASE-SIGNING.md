# Release signing

Every release carries three assets besides the binaries:

- `SHA256SUMS` — one `sha256sum`-format line per binary.
- `SHA256SUMS.sig` — a base64 Ed25519 signature over the exact
  bytes of `SHA256SUMS`, made with the offline release key.
- The binaries themselves.

The app's updater refuses any release whose `SHA256SUMS.sig` is
missing or does not verify against the public key embedded in the
app (`RELEASE_SIGNING_PUBKEY` in `crates/core/src/update.rs`).
Checksums alone only prove a download was not corrupted in
transit; the signature is what proves the release came from the
maintainer.

## The key

The private key is an Ed25519 key held offline by the maintainer.
It is never committed, never uploaded, and never present on the
machine that builds release binaries. Losing it means updates can
no longer be signed (a new key requires a new app release carrying
the new public key); leaking it means anyone can sign updates, so
keep an offline backup and treat it like a password.

Generate a replacement key pair (only when rotating):

```sh
openssl genpkey -algorithm ED25519 -out zaprust-release-ed25519.pem
openssl pkey -in zaprust-release-ed25519.pem -pubout -out zaprust-release-ed25519.pub.pem
# Raw 32-byte public key for RELEASE_SIGNING_PUBKEY:
openssl pkey -in zaprust-release-ed25519.pem -pubout -outform DER | tail -c 32
```

## Signing a release

After the release binaries and `SHA256SUMS` are final:

```sh
openssl pkeyutl -sign -inkey zaprust-release-ed25519.pem -rawin \
  -in SHA256SUMS -out SHA256SUMS.sig.bin
base64 -w0 SHA256SUMS.sig.bin > SHA256SUMS.sig
```

Upload `SHA256SUMS.sig` to the release alongside `SHA256SUMS`
before announcing the release. An unsigned release is visible on
the releases page but every installed copy will refuse to update
to it.
