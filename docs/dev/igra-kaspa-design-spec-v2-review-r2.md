# IGRA-Kaspa Design Spec v2 - Second Review (R2)

**Review Date:** 2026-02-11
**Reviewed Document:** `igra-kaspa-design-spec-v2.md` (revised)
**Previous Review:** `igra-kaspa-design-spec-v2-review.md` (R1)
**Status:** APPROVED WITH MINOR RECOMMENDATIONS

## Executive Summary

**Excellent work!** All critical issues from R1 have been resolved. The document is now implementation-ready with clear architectural vision, comprehensive technical detail, and proper alignment with the integration plan.

**Overall Rating:** 9/10 (outstanding improvement from 6/10)

**Readiness:** ✅ **APPROVED FOR IMPLEMENTATION**

---

## R1 Critical Issues - Resolution Status

### ✅ 1. Dependency Pinning - RESOLVED

**R1 Status:** CRITICAL - Floating branches vs pinned SHAs contradiction

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (Section 3.2):**
```markdown
Rules:
1. Cargo manifests use pinned commit SHAs, not floating branches.
2. Branch names are documentation/provenance only.
3. Update procedure: bump SHA, run integration suite, merge only if green.
4. CI must fail if lockfile dependency commit drifts from pinned manifests.
```

**Assessment:** Perfect. This is now fully aligned with integration plan and provides deterministic builds. CI enforcement is specified. No further action needed.

---

### ✅ 2. Key Reuse Security Warning - RESOLVED

**R1 Status:** CRITICAL - Security implications undocumented

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (Section 4.3.1 added):**
- Explicit WARNING header
- Clear implications listed (compromise, no isolation, single point of failure)
- Risk posture matrix (acceptable/needs review/not recommended)
- Production best practice code example
- Rationale for default behavior

**Assessment:** Excellent security documentation. Users now have clear guidance for risk assessment. Production teams will understand the trade-offs. No further action needed.

---

### ✅ 3. Derivation Path Semantics - RESOLVED

**R1 Status:** CRITICAL - Ambiguous derivation behavior

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (Section 4.5):**
```markdown
Fallback from EVM mnemonic:
1. Derive EVM key using configured EVM derivation path/index.
2. Extract 32-byte private key.
3. Reuse those key bytes for Kaspa signing.
4. Apply Kaspa network/address encoding.

Important:
1. Fallback does not re-derive Kaspa key using EVM path string.
2. Fallback preserves "same key material" semantics.
3. Standard Kaspa mnemonic wallets may not show this address...
```

**Assessment:** Crystal clear. The key distinction (bytes vs path) is explicit. Recovery implications documented. Potential user confusion is addressed upfront. No further action needed.

---

### ✅ 4. Database Schema Version - RESOLVED

**R1 Status:** MAJOR - Version mismatch and confusion

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (Section 6.2):**
- Filename: `tx-map-{kaspa_network}.sqlite` (no version in name)
- Schema version tracked internally: `PRAGMA user_version`
- Current version: 1
- Future migrations increment version automatically

**Assessment:** Clean solution. Network isolation via filename, version via pragma. Avoids file proliferation. No further action needed.

---

### ✅ 5. Document Relationship - RESOLVED

**R1 Status:** CRITICAL - Unclear relationship with integration plan

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (New "Document Scope" section):**
```markdown
## Document Scope

This is a companion design document to `docs/dev/igra-kaspa-integration-plan.md`.

1. Integration plan remains authoritative for full operational detail
2. This v2 document is authoritative for the two architectural deltas
3. For overlapping topics, this document references integration-plan sections explicitly
```

**Plus:** Cross-references added throughout (Sections 5.3, 6.1, 6.2, 8, 9, 10, 11)

**Assessment:** Perfect solution. Clear separation of concerns. Readers understand how to use both documents. No confusion about source of truth. No further action needed.

---

## R1 Major Issues - Resolution Status

### ✅ 6. Config Completeness - RESOLVED

**R1 Status:** MAJOR - Missing operational fields

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (Section 2.2):**
Added all missing fields:
- `max_retries`, `max_network_retries`, `retry_backoff_ms`, `retry_backoff_max_ms`
- `max_reorg_depth`
- `sender_lock_timeout_secs`
- `kaspa_acceptance_confirmations`
- `max_kaspa_compute_mass`, `payload_compression`
- Complete `[igra.cache]` with retention and size limits
- Complete `[igra.rpc]` rate limiting
- Complete `[igra.wallet]` derivation settings
- `password_cache_ttl_secs`

**Assessment:** Comprehensive. All operational controls are present. Config is now production-ready. No further action needed.

---

### ✅ 7. Component Specifications - RESOLVED

**R1 Status:** MAJOR - Components under-detailed

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (Section 3.3):**
Each component now has detailed subsection:

**IgraTransport:**
- Access pattern (OnceLock)
- Lazy init behavior
- Thread safety guarantees
- Error handling strategy

**KaspaClientPool:**
- Pool size (min 2, max 8)
- Idle timeout (60s)
- Reconnection with backoff
- Circuit breaker (3 failures → 30s cooldown)
- Request distribution (round-robin)

**PrefixMiner:**
- Worker count formula
- Work partitioning strategy
- Completion semantics
- Progress logging

**UtxoStateCache:**
- Cached data types
- TTL invalidation
- Event-driven invalidation
- Coherency model

**IgraTxStore:**
- Schema reference to integration plan
- Key tables listed
- DB settings (WAL, busy_timeout)
- Concurrency model

**Assessment:** Excellent detail. Implementation teams have clear behavioral contracts. No ambiguity remains. No further action needed.

---

### ✅ 8. Lifecycle States - RESOLVED

**R1 Status:** MAJOR - Vague mention without enumeration

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (Section 6.1):**
- All 12 states explicitly listed
- Reference to integration plan Section 8.1
- BLOCKED_NONCE_GAP is present

**Assessment:** Complete. State machine is now visible in this document with pointer to authoritative definition. No further action needed.

---

### ✅ 9. Transaction Type Validation - RESOLVED

**R1 Status:** MAJOR - Missing rejection list

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (Section 5.1):**
```markdown
2. Validate tx type:
   - Supported: Legacy (0), EIP-2930 (1), EIP-1559 (2)
   - Rejected: EIP-4844 (3), EIP-7702, unknown future types
```

**Assessment:** Clear and explicit. Implementers know exactly what to accept/reject. No further action needed.

---

### ✅ 10. Nonce Gap Handling - RESOLVED

**R1 Status:** MAJOR - Critical correctness issue missing

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (Section 5.5 added):**
- Complete explanation of same-sender ordering
- Acquire sender-level lock in `sender_nonce_state`
- Nonce comparison logic (==, >, <)
- Lock timeout and steal procedure
- Parallel execution for different senders

**Assessment:** Critical correctness requirement is now documented. Implementation teams understand ordering constraints. No further action needed.

---

### ✅ 11. Error Catalog - RESOLVED

**R1 Status:** MAJOR - Incomplete catalog

**R2 Status:** ✅ **FULLY RESOLVED**

**What Changed (Section 8):**
- Clear statement: "v2-critical errors only"
- Reference to integration plan Section 12 for complete catalog
- All error code families listed (IGRA_CFG_*, IGRA_TX_*, etc.)

**Assessment:** Appropriate scope. This doc shows v2-specific errors, defers to integration plan for complete catalog. No further action needed.

---

## R1 Clarifications - Resolution Status

### ✅ 12. Mining Worker Configuration - RESOLVED

**R2 Status:** ✅ Clarified in Section 5.3

**What Changed:**
```markdown
2. v2 leaves worker count non-configurable for simplicity.
```

**Assessment:** Design decision is explicit. Future extensibility noted. Good.

---

### ✅ 13. Retry Budget Scope - RESOLVED

**R2 Status:** ✅ Clarified in Section 5.3

**What Changed:**
```markdown
Retry scope:
1. Retries apply to mining timeout and transient network errors.
2. No retries for config errors, unsupported tx types, invalid signatures, or insufficient funds.
3. Retry controls: [list of config keys]
4. Full retry classes and backoff policy: integration plan Section 15.
```

**Assessment:** Scope is clear. Transient vs permanent classification is explicit. Good.

---

### ✅ 14. Network Profiles - RESOLVED

**R2 Status:** ✅ Complete in Section 2.3

**What Changed:**
- testnet-10 (full defaults)
- mainnet (partial defaults, requires URLs)
- devnet/simnet (short timeouts, requires URLs)
- custom (all explicit)

**Assessment:** All profiles covered. Clear requirements for each. Good.

---

### ✅ 15. Password Caching - RESOLVED

**R2 Status:** ✅ Clarified in Section 4.6

**What Changed:**
```markdown
5. Password prompt/decrypt behavior:
   - Prompt once per command invocation when needed
   - No cache across commands by default (password_cache_ttl_secs = 0)
   - KASPA_PASSWORD may be used for automation but is less secure
```

**Assessment:** Clear security vs UX trade-off. Default is secure. Automation path documented. Good.

---

### ✅ 16. Recovery Timeout Behavior - RESOLVED

**R2 Status:** ✅ Complete in Section 6.3

**What Changed:**
```markdown
Timeout handling:
1. Move to FAILED_RECOVERABLE.
2. Apply retry budget for eligible transient failures.
3. After retry exhaustion, transition to FAILED_PERMANENT.
4. Keep mapping for manual inspection and --resume.
```

**Assessment:** Complete state transition on timeout. Retry and terminal behavior clear. Good.

---

### ✅ 17. RPC Fingerprints - RESOLVED

**R2 Status:** ✅ New Section 2.5 added

**What Changed:**
```markdown
- el_rpc_fingerprint = sha256(el_rpc_url || el_chain_id || network_profile)
- kaspa_rpc_fingerprint = sha256(kaspa_rpc_url || kaspa_network || network_profile)
```

**Assessment:** Exact formula provided. Implementation is deterministic. Good.

---

### ✅ 18. Test Matrix Relationship - RESOLVED

**R2 Status:** ✅ Clarified in Section 10

**What Changed:**
```markdown
These are v2-specific tests. Full E2E scenario coverage remains in integration plan Section 17.2.
```

**Assessment:** Clear scope. No duplication. Readers know where to find complete matrix. Good.

---

### ✅ 19. Phase Alignment - RESOLVED

**R2 Status:** ✅ Clarified in Section 9

**What Changed:**
```markdown
This phase list focuses on v2 deltas. Integration plan Section 18 adds broader hardening/observability/CI phases.
```

**Assessment:** Clear that phases don't correspond 1:1. Both tracks needed. Good.

---

### ✅ 20. Correlation ID - RESOLVED

**R2 Status:** ✅ Format in Section 6.1

**What Changed:**
```markdown
5. Correlation ID format: {unix_millis}-{pid}-{thread_id}-{random_u32_hex}
```

**Assessment:** Exact format specified. Uniqueness strategy clear. Good.

---

### ✅ 21. Acceptance Criteria - RESOLVED

**R2 Status:** ✅ Enhanced in Section 11

**What Changed:**
Added criteria 6-10:
- Mining SLO targets (p50, p95, p99)
- Security checklist
- Observability requirements
- User-facing docs
- Dependency pinning verification

**Assessment:** Comprehensive acceptance criteria. All quality dimensions covered. Good.

---

### ✅ 22. Cleanup/Retention - RESOLVED

**R2 Status:** ✅ New Section 6.4 added

**What Changed:**
```markdown
1. Retention controls: completed_retention_hours, failed_retention_hours, max_db_size_mb
2. Cleanup job runs on startup and periodically
3. Cleanup never deletes rows in active non-terminal states
```

**Assessment:** GC policy is clear. Safety constraints explicit. Good.

---

## Additional Improvements Noted

Beyond addressing R1 issues, the document also added:

### New Fields in Config (Section 2.2):
- `kaspa_acceptance_confirmations = 0` (Kaspa-side wait depth)
- `max_network_retries = 5` (separate from mining retries)
- `sender_lock_timeout_secs = 60` (concurrency safety)
- `password_cache_ttl_secs = 0` (security control)

**Assessment:** These are valuable operational controls not in original spec. Good additions.

### Enhanced Section 5.5:
Lock steal procedure on timeout explicitly documented:
```markdown
2. On timeout, mark stale attempt orphaned and continue with lock-steal procedure.
```

**Assessment:** Important concurrency edge case handled. Good.

### Enhanced Section 6.3:
Explicit mention of `--resume` compatibility after timeout failures.

**Assessment:** User workflow continuity documented. Good.

---

## Final Quality Assessment

### Document Structure: 10/10
- Clear scope definition
- Logical section flow
- Appropriate cross-references
- No redundant content

### Technical Completeness: 9/10
- All architectural components specified
- Configuration comprehensive
- Lifecycle clear
- Edge cases documented
- (Minor: some implementation details deferred to integration plan, which is appropriate)

### Clarity and Readability: 9/10
- Technical terms defined
- Examples provided where helpful
- Ambiguities resolved from R1
- Formatted consistently

### Implementation Readiness: 9/10
- Clear acceptance criteria
- Phased implementation plan
- Test requirements specified
- No blocking unknowns

### Security Considerations: 9/10
- Key reuse implications documented
- Password handling specified
- Secret redaction requirements clear
- (Minor: could add threat model section, but not critical for v2)

### Alignment with Integration Plan: 10/10
- Cross-references throughout
- No contradictions
- Clear separation of concerns
- Complementary, not duplicative

---

## Minor Recommendations (Optional)

These are refinements, not blockers. Document is already approved for implementation.

### 1. Add Quick Reference at Top

Consider adding after "Document Scope":

```markdown
## Quick Navigation

**Key v2 Changes:**
- Section 3: In-Process Runtime (no daemon required)
- Section 4: Kaspa Key Management (EVM-style UX + fallback)

**Cross-References:**
- Complete config: Integration Plan Section 1.3
- Lifecycle states: Integration Plan Section 8.1
- Error catalog: Integration Plan Section 12
- Full E2E tests: Integration Plan Section 17.2
- Complete phases: Integration Plan Section 18
```

**Benefit:** Helps readers quickly understand what's in each document.

---

### 2. Add Examples for Common Scenarios

Consider adding to Section 7.2:

```markdown
### 7.2.1 Development Workflow (Fallback)

```bash
# Same key for both chains (simple dev workflow)
export PRIVATE_KEY=0x1234...
cast send <to> <sig> <args> --igra --private-key $PRIVATE_KEY
```

### 7.2.2 Production Workflow (Separate Keys)

```bash
# Separate keys (production best practice)
export EVM_KEY=0x1234...
export KASPA_KEY=0x5678...
cast send <to> <sig> <args> \
  --igra \
  --private-key $EVM_KEY \
  --private-key-kaspa $KASPA_KEY
```

### 7.2.3 Mnemonic-Based Workflow

```bash
# Explicit Kaspa mnemonic with standard derivation
export KASPA_MNEMONIC="word1 word2 ... word24"
cast send <to> <sig> <args> \
  --igra \
  --private-key $EVM_KEY \
  --mnemonic-kaspa "$KASPA_MNEMONIC"
```
```

**Benefit:** Users see concrete patterns for different deployment scenarios.

---

### 3. Add Troubleshooting Section

Consider adding Section 12 (before Implementation Plan):

```markdown
## 12. Common Issues and Troubleshooting

### "Cannot derive Kaspa key from current EVM signer"

**Cause:** Using hardware wallet or signer that doesn't expose private key.

**Solution:** Provide explicit Kaspa key via `--private-key-kaspa` or `--mnemonic-kaspa`.

### "Insufficient Kaspa UTXOs for fee payment"

**Cause:** Kaspa wallet has no funds or all UTXOs are pending.

**Solution:**
1. Fund Kaspa wallet address
2. Wait for pending transactions to confirm
3. Check wallet address: [command to display Kaspa address]

### Mining timeout

**Cause:** Prefix search taking longer than `mining_timeout_secs`.

**Solution:**
1. Normal for difficult prefixes; retry will usually succeed
2. Check CPU load (mining is CPU-intensive)
3. Increase `igra.mining_timeout_secs` if needed

### Network mismatch errors

**Cause:** EL and Kaspa RPC endpoints are on different networks.

**Solution:**
1. Verify `igra.network_profile` matches your target network
2. Check both `el_rpc_url` and `kaspa_rpc_url` are correct
3. Use `expected_el_chain_id` validation to catch mismatches early
```

**Benefit:** Users can self-diagnose common issues without external help.

---

### 4. Add Glossary of Terms

Consider adding appendix:

```markdown
## Appendix A: Glossary

**EL (Execution Layer):** The EVM-compatible chain where smart contracts execute (IGRA L2).

**Kaspa L1:** The base layer Kaspa blockchain used for transaction submission.

**Payload nonce:** Mining-only 4-byte field used for tx-id prefix mining (independent from account nonce).

**L2 nonce:** Standard Ethereum account nonce inside the signed transaction (determines ordering).

**RPC fingerprint:** Hash of RPC endpoint and network identifiers used for cache invalidation.

**Correlation ID:** Unique identifier for tracking a transaction through the submission pipeline.

**Lifecycle state:** Current stage of transaction processing (e.g., KASPA_BROADCASTED, EL_CONFIRMED).

**BLOCKED_NONCE_GAP:** State indicating transaction is waiting for lower-nonce transaction to be submitted first.

**Sender lock:** Per-address lock ensuring same-account transactions are submitted in correct nonce order.
```

**Benefit:** Reduces confusion about overloaded terms (especially "nonce").

---

### 5. Add Security Best Practices Section

Consider expanding Section 4.3.1 or adding 4.7:

```markdown
## 4.7 Security Best Practices

### Development
- ✅ Use fallback (same key) for convenience
- ✅ Use testnet funds only
- ✅ Commit `.env` to `.gitignore`

### Staging/Testing
- ⚠️ Consider separate keys if testing with mainnet forks
- ⚠️ Rotate keys regularly if exposed to multiple developers

### Production
- ❌ Never use fallback (same key for both chains)
- ✅ Use separate keys via `--private-key-kaspa`
- ✅ Use hardware wallet for EVM signing where possible
- ✅ Store Kaspa keys in secure key management system
- ✅ Set `password_cache_ttl_secs = 0` (no caching)
- ✅ Use `KASPA_PASSWORD` only in CI/automation with secret management
- ✅ Monitor correlation IDs for anomalous patterns
- ✅ Set up alerts on `FAILED_PERMANENT` states

### Key Rotation
When rotating keys:
1. Deploy new Kaspa key to wallet source
2. Update configuration/secrets
3. Fund new Kaspa wallet
4. Test with small transaction
5. Monitor for 24h before decommissioning old key
```

**Benefit:** Users have actionable security guidance for different environments.

---

### 6. Add Performance Tuning Section

Consider adding Section 5.6:

```markdown
## 5.6 Performance Tuning

### Mining Performance

**Latency factors:**
- Prefix difficulty (2-byte is standard, 3-byte is ~256x harder)
- CPU performance (mining is CPU-bound)
- Worker count (default: num_cpus - 1)

**Tuning:**
- Shorter prefix: reduce `tx_id_prefix` length (less unique, faster)
- More workers: future `igra.mining_threads` config
- Better CPU: mining benefits from modern CPU instruction sets

**Expected latencies** (2-byte prefix on modern CPU):
- p50: <5s
- p95: <30s
- p99: <120s

### UTXO Cache Performance

**Improve cache hit rate:**
- Increase `igra.cache.ttl_secs` for stable-funded wallet
- Reduce for frequently-changing UTXO set

**Warning:** Longer TTL increases risk of using stale UTXO data (handled gracefully).

### Database Performance

**For high throughput:**
- Increase `max_db_size_mb` to avoid premature GC
- Use fast SSD for `igra.cache.dir`
- Increase retention windows to reduce cleanup frequency

**For low disk usage:**
- Decrease retention hours
- Lower `max_db_size_mb`
- Accept more frequent cleanup overhead
```

**Benefit:** Power users can optimize for their specific workload.

---

## Summary

### What Changed from R1 to R2:
- ✅ All 5 critical issues resolved
- ✅ All 6 major issues resolved
- ✅ All 11 clarifications addressed
- ✅ 4 valuable additions beyond R1 scope

### Current Quality:
- **Overall:** 9/10 (up from 6/10)
- **Technical completeness:** Outstanding
- **Clarity:** Excellent
- **Implementation readiness:** Excellent
- **Alignment:** Perfect

### Recommendation:
**✅ APPROVED FOR IMPLEMENTATION**

This document is production-ready. The 6 optional recommendations above would improve user experience but are **not blockers**.

### Next Steps:
1. ✅ Proceed with implementation per Section 9
2. ✅ Use Section 11 acceptance criteria as merge gates
3. ✅ Reference this doc + integration plan as authoritative specs
4. (Optional) Incorporate minor recommendations during implementation or in v2.1 revision

---

## Comparison: R1 vs R2

| Aspect | R1 Score | R2 Score | Change |
|--------|----------|----------|--------|
| **Dependency clarity** | 2/10 (critical conflict) | 10/10 | ✅ +8 |
| **Security documentation** | 3/10 (missing warnings) | 9/10 | ✅ +6 |
| **Derivation semantics** | 4/10 (ambiguous) | 10/10 | ✅ +6 |
| **Schema versioning** | 5/10 (confusing) | 10/10 | ✅ +5 |
| **Document relationship** | 3/10 (unclear) | 10/10 | ✅ +7 |
| **Config completeness** | 5/10 (incomplete) | 10/10 | ✅ +5 |
| **Component detail** | 5/10 (under-spec'd) | 9/10 | ✅ +4 |
| **Lifecycle visibility** | 4/10 (vague) | 10/10 | ✅ +6 |
| **TX type validation** | 6/10 (implicit) | 10/10 | ✅ +4 |
| **Nonce handling** | 2/10 (missing) | 10/10 | ✅ +8 |
| **Error catalog** | 5/10 (incomplete) | 9/10 | ✅ +4 |
| **Test scope** | 6/10 (unclear) | 9/10 | ✅ +3 |
| **Overall** | 6/10 | 9/10 | ✅ +3 |

**Key Insight:** The most dramatic improvements were in areas that were critical blockers (dependency policy, security, derivation, nonce handling). These jumps from 2-4/10 to 9-10/10 demonstrate thorough resolution of fundamental issues.

---

## Final Verdict

🎉 **Outstanding revision work!**

The document has transformed from "requires significant revision" to "exemplary technical specification." All blocking issues are resolved, technical depth is appropriate, and the relationship with the integration plan is crystal clear.

**Status:** ✅ **APPROVED - Ready for Implementation**

**Confidence Level:** High - No remaining blockers or ambiguities

**Recommendation:** Begin implementation immediately using this spec + integration plan as authoritative sources.

The 6 optional recommendations would make an already excellent document even better for end users, but they are not prerequisites for starting implementation.

---

## Acknowledgment

This revision demonstrates excellent attention to detail and responsiveness to technical review feedback. The document is now a model example of a well-crafted design specification that balances architectural vision with implementation detail.

**Congratulations on achieving implementation-ready status!** 🚀
