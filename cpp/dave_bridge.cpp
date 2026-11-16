// Implementation over discord/libdave (https://github.com/discord/libdave).
// NOTE: written without the pinned header at hand -> the libdave calls marked VERIFY
// must be matched to the exact release you vendor (git submodule + pin a tag).
#include "dave_bridge.h"
#include <dave/dave.h>      // VERIFY: libdave public C API header
#include <vector>
#include <cstring>

struct AwDave {
    DAVESessionHandle    session  = nullptr;
    DAVEEncryptorHandle  encryptor = nullptr;
    DAVEKeyRatchetHandle pending_ratchet = nullptr;  // staged until EXECUTE_TRANSITION
    uint64_t user_id, channel_id;
    uint16_t version = 0;
};

extern "C" {
AwDave* aw_dave_new(uint64_t u, uint64_t c) {
    auto* d = new AwDave; d->user_id = u; d->channel_id = c;
    d->encryptor = daveEncryptorCreate();                    // VERIFY
    return d;
}
void aw_dave_free(AwDave* d) {
    if (!d) return;
    if (d->encryptor) daveEncryptorDestroy(d->encryptor);    // VERIFY
    if (d->session)   daveSessionDestroy(d->session);        // VERIFY
    delete d;
}
int aw_dave_init(AwDave* d, uint16_t v) {
    d->version = v;
    if (v == 0) { daveEncryptorSetPassthroughMode(d->encryptor, true); return 0; }  // VERIFY
    if (!d->session) d->session = daveSessionCreate(nullptr, nullptr, nullptr);     // VERIFY
    std::string uid = std::to_string(d->user_id);
    daveSessionInit(d->session, v, d->channel_id, uid.c_str());                     // VERIFY
    return 0;
}
// ... remaining entry points map 1:1 onto daveSessionSetExternalSender,
// daveSessionGetMarshalledKeyPackage, daveSessionProcessProposals, daveSessionProcessCommit,
// daveSessionProcessWelcome, daveSessionGetKeyRatchet -> daveEncryptorSetKeyRatchet,
// daveEncryptorEncrypt(…, DAVE_MEDIA_TYPE_AUDIO, ssrc, …).  See README "DAVE" section.
}
