//! TLS impersonation profile for the Claude Code subscription path.
//!
//! The real Claude Code CLI is a Node.js process talking to Anthropic through
//! `@anthropic-ai/sdk` over OpenSSL, so its model-path handshake fingerprints as
//! JA4 `t13d1714h1_5b57614c22b0_43ade6aba3df` (Node/OpenSSL, HTTP/1.1 only). Our
//! Cookie/OAuth client sends the `claude-cli/...` User-Agent plus the
//! `x-stainless-*` (`runtime=node`) header set on every `/v1/messages` call, so
//! the handshake MUST look like Node/OpenSSL too — pairing those headers with a
//! browser (Chrome) TLS fingerprint is a self-contradiction Anthropic's edge can
//! correlate trivially (JA3/JA4 ↔ User-Agent mismatch).
//!
//! Values mirror the real CLI capture: ALPN `http/1.1` only, GREASE disabled,
//! TLS 1.2–1.3, the OpenSSL default cipher list, curves `X25519:P-256:P-384`,
//! and the Node sigalgs list.

use wreq::tls::{AlpnProtocol, TlsOptions, TlsVersion};

/// Build the Node/OpenSSL emulation the real Claude Code CLI presents on the
/// `/v1/messages` model path. Applied to the Cookie/OAuth (subscription)
/// client, which authenticates as `claude-cli`.
pub(crate) fn claude_code_emulation() -> wreq::Emulation {
    let tls = TlsOptions::builder()
        .alpn_protocols(vec![AlpnProtocol::HTTP1])
        .grease_enabled(false)
        .min_tls_version(TlsVersion::TLS_1_2)
        .max_tls_version(TlsVersion::TLS_1_3)
        .cipher_list(
            "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:\
             ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
             ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
             ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
             ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:\
             ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:\
             AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA",
        )
        .curves_list("X25519:P-256:P-384")
        .sigalgs_list(
            "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:\
             ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:\
             rsa_pss_rsae_sha512:rsa_pkcs1_sha512:rsa_pkcs1_sha1",
        )
        .build();
    wreq::Emulation::builder().tls_options(tls).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_builds() {
        // Smoke test: the TLS option strings are accepted by the builder and the
        // emulation is constructible (a malformed cipher/curve/sigalg token would
        // surface here rather than at first connect).
        let _ = claude_code_emulation();
    }
}
