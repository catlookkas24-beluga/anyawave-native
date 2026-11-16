// STUB implementation of the DAVE bridge (no libdave needed).
// Purpose: let the whole project build and the Lavalink-compatible API run (e.g. on Render).
// Behaviour: DAVE is "not supported" -> frames are passed through unencrypted (version 0).
// Discord voice channels that REQUIRE DAVE will not accept this audio.
// TODO(real): replace with calls into discord/libdave once vendored at third_party/libdave.
#include "dave_bridge.h"
#include <cstring>
#include <new>

struct AwDave {
    uint64_t user_id, channel_id;
    uint16_t version = 0;
};

extern "C" {

AwDave* aw_dave_new(uint64_t user_id, uint64_t channel_id) {
    auto* d = new (std::nothrow) AwDave;
    if (d) { d->user_id = user_id; d->channel_id = channel_id; }
    return d;
}
void aw_dave_free(AwDave* d) { delete d; }

int aw_dave_init(AwDave* d, uint16_t version) { (void)version; d->version = 0; return 0; }

void aw_dave_set_external_sender(AwDave*, const uint8_t*, size_t) {}

int aw_dave_key_package(AwDave*, uint8_t*, size_t) { return 0; }

int aw_dave_process_proposals(AwDave*, const uint8_t*, size_t, const uint64_t*, size_t,
                              uint8_t*, size_t, size_t* commit_len) {
    if (commit_len) *commit_len = 0;
    return 0;
}

int aw_dave_process_commit(AwDave*, const uint8_t*, size_t) { return -1; }

int aw_dave_process_welcome(AwDave*, const uint8_t*, size_t, const uint64_t*, size_t) { return -1; }

void aw_dave_execute_transition(AwDave*, uint16_t) {}

size_t aw_dave_max_ct(AwDave*, size_t plain_len) { return plain_len; }

int aw_dave_encrypt(AwDave*, uint32_t, const uint8_t* in, size_t n, uint8_t* out, size_t cap) {
    if (cap < n) return -1;
    std::memcpy(out, in, n);          // passthrough
    return (int)n;
}

uint32_t aw_dave_privacy_code_len(void) { return 0; }

} // extern "C"
