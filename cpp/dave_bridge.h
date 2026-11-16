// Stable C ABI between Rust and Discord's libdave (C++, MLS). Rust never sees libdave types.
#pragma once
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
typedef struct AwDave AwDave;

AwDave* aw_dave_new(uint64_t user_id, uint64_t channel_id);
void    aw_dave_free(AwDave*);
// protocol version from SESSION_DESCRIPTION / PREPARE_EPOCH. 0 => passthrough (no E2EE)
int     aw_dave_init(AwDave*, uint16_t version);
void    aw_dave_set_external_sender(AwDave*, const uint8_t* d, size_t n);
// returns bytes written into out (caller's buffer), or -1
int     aw_dave_key_package(AwDave*, uint8_t* out, size_t cap);
// out = commit||welcome? ; *commit_len tells where commit ends. returns total or 0 if nothing to send, -1 err
int     aw_dave_process_proposals(AwDave*, const uint8_t* d, size_t n,
                                  const uint64_t* recognized_ids, size_t n_ids,
                                  uint8_t* out, size_t cap, size_t* commit_len);
int     aw_dave_process_commit(AwDave*, const uint8_t* d, size_t n);   // 0 ok, <0 -> send op31 + re-init
int     aw_dave_process_welcome(AwDave*, const uint8_t* d, size_t n, const uint64_t* ids, size_t n_ids);
void    aw_dave_execute_transition(AwDave*, uint16_t transition_id);   // swap key ratchet in
// Encrypt ONE opus frame. out cap >= aw_dave_max_ct(len). returns written bytes, -1 err
size_t  aw_dave_max_ct(AwDave*, size_t plain_len);
int     aw_dave_encrypt(AwDave*, uint32_t ssrc, const uint8_t* in, size_t n, uint8_t* out, size_t cap);
uint32_t aw_dave_privacy_code_len(void);
#ifdef __cplusplus
}
#endif
