# IGRA-Kaspa Design Spec v2 - Critical Review

**Review Date:** 2026-02-11
**Reviewed Document:** `igra-kaspa-design-spec-v2.md`
**Reviewer:** Architecture Review
**Status:** REQUIRES REVISION - Critical issues must be resolved before implementation

## Executive Summary

The v2 design spec provides clear architectural vision for in-process kaswallet runtime and EVM-style key UX. However, it contains **critical contradictions** with the integration plan and **missing security considerations** that must be resolved before implementation.

**Overall Rating:** 6/10 (requires significant revision)

**Readiness:** ⛔ BLOCKED on critical issues

## Critical Issues (MUST FIX)

### 1. Dependency Policy Contradiction 🚨

**Severity:** CRITICAL - BLOCKS IMPLEMENTATION

**Location:** Section 3.2

**Issue:**
- **This document says:** "Use GitHub branches (not pinned SHAs in v2)"
- **Integration plan says:** "Pin immediately to commit SHA"
- These are **mutually exclusive** and will cause CI/deployment chaos

**Impact:**
- Non-reproducible builds
- CI flakiness when upstream changes
- Impossible to debug which version is deployed
- Violates deterministic build requirements

**Required Fix:**
```diff
## 3.2 Dependency Source Policy

- Use GitHub branches (not pinned SHAs in v2):
+ Use GitHub branches to select initial commit, then pin to SHA:

1. `IgraLabs/kaswallet`, branch `roman/utxo-perf-opt`
2. `IgraLabs/rusty-kaspa`, branch `roman/devel`

- Track branch head in this integration phase; add CI guard to record resolved commit in build logs.
+ Pin to commit SHA immediately in Cargo.toml (not floating branch refs).
+ Branch names are for provenance documentation only.
+ Update procedure: bump SHA → run integration suite → merge if green.
+ CI must fail if lockfile SHA drifts from Cargo.toml pinned SHA.
```

**Action:** Align with integration plan Section 8 immediately.

---

### 2. Key Reuse Security Implications ⚠️

**Severity:** CRITICAL - SECURITY ISSUE

**Location:** Section 4.3

**Issue:**
Using the same private key for both EVM (ECDSA) and Kaspa (Schnorr) has undocumented security implications that users must understand before production use.

**Security Concerns:**
1. Single key compromise exposes assets on both chains
2. No cryptographic isolation between execution environments
3. Different signature schemes (ECDSA secp256k1 vs Schnorr secp256k1)
4. May violate organizational security policies requiring key separation
5. Increased attack surface (more places key is used = more exposure)

**Production Risk:**
Users may unknowingly deploy with shared keys, violating their own security requirements.

**Required Fix:**

Add new section **4.3.1 Security Considerations for Key Fallback:**

```markdown
## 4.3.1 Security Considerations for Key Fallback

⚠️ **SECURITY WARNING**: Default behavior uses the same private key for both EVM and Kaspa signing.

**Implications:**
- Compromise on either chain exposes assets on the other
- No cryptographic isolation between chains
- Single point of failure for both execution environments
- May not meet enterprise security requirements

**Risk Assessment:**
- ✅ **Acceptable:** Development, testing, testnets, personal wallets
- ⚠️ **Evaluate:** Production deployments with significant value
- ❌ **Not Recommended:** Enterprise, custodial, or high-security environments

**Production Best Practice:**
```bash
# Use separate keys for production
cast send <to> <sig> <args> \
  --igra \
  --private-key $EVM_KEY \
  --private-key-kaspa $KASPA_KEY  # Separate key
```

**Rationale for Default:**
This default prioritizes developer experience for common workflows where the same
entity controls both key uses and convenience outweighs isolation benefits.
```

**Action:** Add security warning section before implementation starts.

---

### 3. Derivation Path Semantics Unclear 🔴

**Severity:** CRITICAL - INTEROPERABILITY ISSUE

**Location:** Section 4.5

**Issue:**
The document states conflicting derivation behaviors:

> "Default path: `m/44'/111111'/0'/0/0`" (standard Kaspa BIP-44)
>
> "When fallback from EVM mnemonic is used: Reuse the EVM derivation path exactly"

**Problem:**
EVM typically uses `m/44'/60'/0'/0/X`. If you use this path string to derive Kaspa keys:
- **Non-standard Kaspa derivation** (should be coin type 111111, not 60)
- **Breaks interoperability** with Kaspa ecosystem wallets
- **External tools won't find** these addresses
- **Recovery nightmare** if user loses Foundry but has mnemonic

**Ambiguity:**
Does "reuse derivation path exactly" mean:
- **Interpretation A:** Use EVM path string (`m/44'/60'/0'/0/0`) to derive Kaspa key?
- **Interpretation B:** Derive EVM key, extract 32 bytes, use those bytes for Kaspa?

**Required Fix:**

```markdown
## 4.5 Derivation Defaults and Fallback Semantics

### Explicit Kaspa Mnemonic (--mnemonic-kaspa)

When Kaspa mnemonic is explicit and no derivation path is provided:

1. Default path: `m/44'/111111'/0'/0/0` (BIP-44 standard, coin_type=111111 for Kaspa)
2. Default index: `0`

This produces standard Kaspa addresses compatible with ecosystem wallets.

### Fallback from EVM Mnemonic

When no explicit Kaspa mnemonic is provided and EVM mnemonic is available:

**Implementation:**
1. Derive EVM private key using configured EVM derivation path
   - Example: `m/44'/60'/0'/0/0` → 32-byte secp256k1 private key
2. Extract the raw 32-byte private key
3. **Reuse those same 32 key bytes** for Kaspa Schnorr signing
4. Apply Kaspa address encoding (network prefix, Bech32m)

**Important:** The Kaspa key is derived from the **key bytes**, not by re-deriving
with the EVM path string. This preserves "same signer material" semantics while
avoiding non-standard Kaspa BIP-44 paths.

**Consequence:**
- EVM address: derived from `m/44'/60'/0'/0/0`
- Kaspa address: encoded from **same key bytes** (not derived via path)
- Standard Kaspa wallets will NOT show this address (non-standard derivation source)
- User must use Foundry or import raw key bytes to recover Kaspa funds

**Recovery Procedure:**
```bash
# If user has only mnemonic and standard Kaspa wallet:
# 1. Use Foundry with --mnemonic to derive key bytes
# 2. Export key bytes
# 3. Import into Kaspa wallet as raw key (not mnemonic derivation)
```

**Trade-off:**
This design prioritizes "same key" semantics over standard BIP-44 compliance.
Users who need standard Kaspa derivation paths should use --mnemonic-kaspa explicitly.
```

**Action:** Clarify derivation semantics with explicit examples and recovery implications.

---

### 4. Database Schema Version Confusion 🔴

**Severity:** MAJOR - CONSISTENCY ISSUE

**Location:** Section 6.2

**Issue:**
- **This document:** `tx-map-v2-{kaspa_network}.sqlite`
- **Integration plan:** `tx-map-v1.sqlite`

**Questions:**
1. Is this v2 of the design spec but v1 of the schema?
2. If schema is v2, what changed from v1?
3. How do we handle migration from v1 to v2?

**Required Fix:**

```markdown
## 6.2 Cache Isolation and Schema Versioning

### Database Naming

DB path includes network for isolation:

- `<cache.dir>/tx-map-{kaspa_network}.sqlite`

Examples:
- `~/.foundry/cache/igra/tx-map-mainnet.sqlite`
- `~/.foundry/cache/igra/tx-map-testnet-10.sqlite`

This prevents testnet/mainnet contamination when switching networks.

### Schema Version Management

Schema version is tracked internally via `PRAGMA user_version`:
- Current: version 1
- Future schema changes increment version
- Migration handled automatically on first open with newer version

Note: File naming does not include version number to avoid proliferation of
legacy files. Version is internal-only.

### Cross-Document Alignment

Schema definition is in integration plan Section 9.1. This document defers to
that specification for table structure and columns.
```

**Action:** Align database naming and clarify schema versioning strategy.

---

## Major Issues (SHOULD FIX)

### 5. Config Section Incomplete

**Severity:** MAJOR

**Location:** Section 2.2

**Issue:**
Config schema is missing fields that integration plan defines:

**Missing Fields:**
- `max_retries`, `retry_backoff_ms`, `retry_backoff_max_ms`
- `max_reorg_depth`
- `max_kaspa_compute_mass`
- `payload_compression`
- `[igra.rpc]` section (rate limiting: `kaspa_qps`, `el_qps`, `burst`)
- `[igra.cache]` retention: `completed_retention_hours`, `failed_retention_hours`, `max_db_size_mb`
- `[igra.wallet]` derivation: `master_path`, `allow_custom_master_path`, `cosigner_index`, `account_count`, `address_gap_limit`

**Impact:**
Config examples won't work for full implementation; missing operational controls.

**Recommended Fix:**

Add note to Section 2.2:
```markdown
## 2.2 Required Config in IGRA Mode

Note: This section shows minimal required fields for v2 architectural changes.
For complete configuration reference including retry policy, rate limiting,
cache retention, and advanced wallet options, see:
- `igra-kaspa-integration-plan.md` Section 1.3

Below is the minimal config subset relevant to v2 changes:
```

**Alternative:** Include complete config from integration plan.

**Action:** Either complete config or add explicit reference to integration plan.

---

### 6. Component Specifications Under-Detailed

**Severity:** MAJOR

**Location:** Section 3.3

**Issue:**
Components are listed but lack behavioral specifications:

#### IgraTransport<T>
- How does it access runtime singleton?
- Thread safety model?
- Multiple transport instances per process?
- Error propagation strategy?

#### KaspaClientPool
- Pool size and connection limits?
- gRPC channel reuse strategy?
- Reconnection and failure handling?
- Concurrent request limits?

#### PrefixMiner
- Worker count configurable or fixed?
- Work-stealing or partition strategy?
- Cancellation on timeout/shutdown?
- Progress reporting mechanism?

#### UtxoStateCache
- What's cached: UTXO set, balances, fee rates?
- Cache invalidation triggers?
- TTL vs event-driven expiry?
- Cross-process cache coherency?

#### IgraTxStore
- Schema reference (no table definition)?
- Required indices for performance?
- WAL mode, busy_timeout, journal_mode settings?
- Concurrent write coordination?

**Recommended Fix:**

Expand Section 3.3 with subsections:

```markdown
## 3.3 Runtime Components

### IgraTransport<T>

Provider-level transport wrapper that intercepts write methods.

**Access Pattern:**
- Obtains runtime via process-global `OnceLock<IgraSubmitRuntime>`
- Lazy initialization on first write method
- Thread-safe: multiple transports share same runtime instance

**Error Handling:**
- IGRA-specific errors mapped to RPC error format
- Non-write methods pass through unchanged

### KaspaClientPool

Connection pool for Kaspa gRPC endpoints.

**Configuration:**
- Pool size: min=2, max=8 concurrent connections
- Idle timeout: 60s
- Reconnection: exponential backoff with jitter
- Circuit breaker: 3 consecutive failures → 30s cooldown

**Concurrency:**
- Round-robin distribution across healthy connections
- Automatic failover on connection errors

### PrefixMiner

CPU-bound worker pool for payload nonce mining.

**Worker Count:**
- Default: `max(1, num_cpus - 1)`
- Not configurable in v2 (future: `igra.mining_threads`)

**Work Distribution:**
- Each worker searches disjoint nonce ranges
- First worker to find valid prefix wins
- All workers terminate on success or timeout

**Observability:**
- Progress logged every 5s (attempts/sec, elapsed)
- Final stats on completion

### UtxoStateCache

In-memory cache for Kaspa wallet state.

**Cached Data:**
- Spendable UTXO set
- Current fee rate estimates
- Wallet balance snapshot

**Invalidation:**
- TTL: 20s (configurable via `igra.cache.ttl_secs`)
- On error: insufficient funds, double-spend conflict
- On endpoint change: RPC fingerprint mismatch

**Persistence:**
- Not persisted across processes
- Each process maintains hot cache independently

### IgraTxStore (SQLite)

Persistent transaction mapping and lifecycle tracking.

**Schema:**
See integration plan Section 9.1 for complete table definitions.

**Key Tables:**
- `tx_map`: (l2_tx_hash PRIMARY KEY, kaspa_tx_id, state, sender, l2_nonce, ...)
- `sender_nonce_state`: (sender PRIMARY KEY, next_expected_nonce, ...)

**Configuration:**
- WAL mode enabled
- `busy_timeout = 5000ms`
- Transactional state updates

**See Also:** Integration plan Sections 9.1-9.4 for detailed persistence semantics.
```

**Action:** Expand component specifications or reference implementation docs.

---

### 7. Missing Lifecycle State Details

**Severity:** MAJOR

**Location:** Section 6.1

**Issue:**
Mentions "lifecycle state" but doesn't enumerate states or reference where they're defined.

Integration plan defines 12 states including critical `BLOCKED_NONCE_GAP` state.

**Recommended Fix:**

```markdown
## 6.1 Persistent Mapping

Store in SQLite:

1. `l2_tx_hash`
2. `kaspa_tx_id`
3. **lifecycle state** (see below)
4. sender, L2 nonce, payload nonce
5. correlation ID
6. timestamps and retry counts
7. RPC fingerprints (EL + Kaspa)

### Lifecycle States

Complete state machine definition in integration plan Section 8.1:
1. RECEIVED_RAW_L2
2. BLOCKED_NONCE_GAP
3. KASPA_UNSIGNED_CREATED
4. KASPA_PREFIX_MINED
5. KASPA_SIGNED
6. KASPA_BROADCASTED
7. KASPA_ACCEPTED
8. EL_INDEXED
9. EL_RECEIPT_INCLUDED
10. EL_CONFIRMED
11. FAILED_RECOVERABLE
12. FAILED_PERMANENT

Critical for v2: `BLOCKED_NONCE_GAP` ensures same-sender transactions are
ordered correctly across concurrent processes (integration plan Section 4.4).
```

**Action:** Reference state machine or include complete list.

---

### 8. Transaction Type Validation Missing

**Severity:** MAJOR

**Location:** Section 5.1

**Issue:**
Flow mentions "typed tx (legacy/2930/1559)" but doesn't list rejected types.

Integration plan explicitly rejects EIP-4844 and EIP-7702.

**Recommended Fix:**

```diff
## 5.1 Write-Path Interception

`IgraTransport<T>` intercepts only `eth_sendRawTransaction`.

Flow:

1. Decode typed tx (legacy/2930/1559).
+    - **Supported:** Legacy (0), EIP-2930 (1), EIP-1559 (2)
+    - **Rejected:** EIP-4844 (3), EIP-7702, future types
+    - Rejection error: "IGRA unsupported transaction type: EIP-4844"
2. Validate tx type, size, chain ID.
...
```

**Action:** Add explicit type validation rules to flow.

---

### 9. Missing Nonce Gap Handling

**Severity:** MAJOR - CORRECTNESS ISSUE

**Location:** Throughout (absent)

**Issue:**
Document doesn't mention concurrent transaction ordering or nonce gap handling.

Integration plan Section 4.4 defines critical `BLOCKED_NONCE_GAP` behavior:
- Same-sender transactions must be ordered by L2 nonce
- Higher nonce waits for lower nonce to be submitted
- Prevents out-of-order submission across processes

**Impact:**
Without this, concurrent sends from same account could violate EVM nonce ordering.

**Recommended Fix:**

Add new section:

```markdown
## 5.5 Concurrent Transaction Ordering

### Nonce Gap Prevention

For same sender address across concurrent processes:

1. Decode sender + L2 nonce from raw tx before Kaspa wrapping
2. Check sender's `next_expected_nonce` in persistent store
3. If `tx.nonce == next_expected_nonce`: proceed to submission
4. If `tx.nonce > next_expected_nonce`: move to `BLOCKED_NONCE_GAP`, wait
5. If `tx.nonce < next_expected_nonce`: treat as replacement/duplicate

**Locking:**
- Sender-level lock during nonce check + state update
- Atomic `sender_nonce_state` table update
- Other senders proceed in parallel (no global lock)

**See:** Integration plan Section 4.4 for complete nonce gap semantics.
```

**Action:** Add nonce gap handling or reference integration plan Section 4.4.

---

### 10. Error Catalog Incomplete

**Severity:** MAJOR

**Location:** Section 8

**Issue:**
Only 5 errors defined; integration plan has 14+ error codes with stable codes.

**Recommended Fix:**

```markdown
## 8. Validation Errors (Minimum Catalog)

Below are v2-specific errors. For complete error catalog with stable error codes
(IGRA_CFG_001, IGRA_TX_001, etc.), see integration plan Section 12.

### Key Resolution Errors (v2)
...existing errors...

### See Also

Complete error catalog: `igra-kaspa-integration-plan.md` Section 12
- Config errors (IGRA_CFG_001-003)
- Signer errors (IGRA_SIG_001)
- Transaction errors (IGRA_TX_001-002)
- Nonce errors (IGRA_NONCE_001)
- Fee errors (IGRA_FEE_001-002)
- Mining errors (IGRA_MINING_001)
- Network errors (IGRA_NET_001-002)
- State errors (IGRA_STATE_001)
- Recovery errors (IGRA_RECOVERY_001)
```

**Action:** Reference complete catalog or include all codes.

---

## Important Clarifications (RECOMMENDED)

### 11. Mining Worker Configuration

**Location:** Section 5.3

**Issue:**
Worker count is hardcoded: `max(1, num_cpus - 1)`

**Questions:**
- Can users override for laptop vs server?
- Single-core systems get 1 worker?
- Should there be `igra.mining_threads` config?

**Recommendation:**
```markdown
## 5.3 Prefix Mining Execution

...existing content...

Worker count default: `max(1, num_cpus - 1)`.
- Not configurable in v2 (simplicity first)
- Future: `igra.mining_threads` config option if needed
- Rationale: Reserve 1 core for OS/RPC to avoid thrashing
```

---

### 12. Retry Budget Scope

**Location:** Section 5.3

**Issue:**
Mentions `igra.max_retries` but:
1. Not in config Section 2.2
2. Which failures get retries?

**Recommendation:**
Add to Section 5.3:
```markdown
Retry policy:
- Applies to: mining timeouts, transient network errors
- Does NOT apply to: config errors, invalid tx type, insufficient funds
- See integration plan Section 15 for complete retry classification
```

---

### 13. Network Profile Incomplete

**Location:** Section 2.3

**Issue:**
Only testnet-10 defaults defined.

**Recommendation:**
```markdown
## 2.3 Network Profile Defaults

### testnet-10
- `el_rpc_url = https://galleon-testnet.igralabs.com:8545`
- `kaspa_rpc_url = grpc://stage-roman.igralabs.com:16210`
- `kaspa_network = testnet-10`
- `el_confirmations = 1`

### mainnet
- `kaspa_network = mainnet`
- `el_confirmations = 12`
- **Requires:** explicit `el_rpc_url` and `kaspa_rpc_url`

### devnet / simnet
- Short timeouts for local testing
- **Requires:** explicit RPC URLs

### custom
- All required keys must be explicit (no defaults)
```

---

### 14. Password Caching Lifetime

**Location:** Section 4.6 (missing detail)

**Issue:**
Unclear if password is cached, re-prompted, or session-based.

**Recommendation:**
Add to Section 4.6:
```markdown
## 4.6 Security Requirements

...existing requirements...

### Password Handling

- Keystore password prompted once per command invocation
- Not cached across commands (no session state)
- Cleared from memory after keystore decryption
- Can be provided via `KASPA_PASSWORD` env for automation (less secure)
```

---

### 15. Recovery Timeout Behavior

**Location:** Section 6.3

**Issue:**
Says "poll until `el_receipt_timeout_secs` expires" but doesn't specify what happens after timeout.

**Recommendation:**
```markdown
## 6.3 Recovery Window

When a transaction reaches `KASPA_BROADCASTED`, poll EL receipt until `el_receipt_timeout_secs` expires.

Backoff schedule:
1. 1s
2. 2s
3. 4s
4. 8s
5. capped at 10s intervals thereafter

**On Timeout:**
- Move to `FAILED_RECOVERABLE`
- Apply retry budget (if `max_retries` configured)
- After retry exhaustion: `FAILED_PERMANENT`
- User must manually investigate and potentially resubmit
```

---

## Minor Issues

### 16. Test Matrix vs Integration Plan

**Issue:** 10 tests here vs 18 scenarios in integration plan.

**Recommendation:** Clarify relationship:
```markdown
## 10. Required Test Matrix

Note: These are v2-specific tests focusing on in-process runtime and key resolution.
For complete E2E scenario coverage (reorg, GC, network partitions), see
integration plan Section 17.2.
```

---

### 17. Phase Alignment

**Issue:** 5 phases here vs 7 in integration plan.

**Recommendation:** Add note:
```markdown
## 9. Implementation Plan

Note: These phases focus on v2 architectural changes (in-process runtime, key UX).
Integration plan defines additional phases for observability, hardening, and E2E CI.
Phase numbers do not correspond directly; both tracks must be completed for v2.
```

---

### 18. Correlation ID Format

**Issue:** Mentioned in 6.1 but format not defined.

**Recommendation:**
```markdown
## 6.1 Persistent Mapping

...existing fields...

5. **correlation ID** (format: `{unix_millis}-{pid}-{thread_id}-{random_u32_hex}`)
```

---

### 19. Acceptance Criteria Incomplete

**Issue:** Section 11 missing performance, security, observability requirements.

**Recommendation:**
Add to Section 11:
```markdown
## 11. Acceptance Criteria for v2

v2 is complete only when all are true:

1-5. ...existing criteria...

6. Mining performance meets SLOs: p50<5s, p95<30s, p99<120s (2-byte prefix)
7. Security checklist complete: key zeroization, permission checks, no secret logging
8. Observability: metrics + structured logs operational (integration plan Section 13)
9. Documentation: user guide for IGRA mode completed
10. No regression for non-IGRA (standard EVM) command behavior.
```

---

## Document Relationship Issues

**Critical Meta-Problem:** These two documents have unclear relationship.

### Current State:
- **This document:** High-level architecture, v2 changes (in-process, key UX)
- **Integration plan:** Detailed implementation, config, lifecycle, testing

### Issues:
1. No cross-references
2. Conflicting details (dependencies, database naming)
3. Overlapping scope (both define config, testing, phases)
4. Unclear which is "source of truth"

### Recommendations:

**Option A - Make this v2 doc a delta document:**

Add to beginning:
```markdown
## Document Scope

This v2 design spec defines two major architectural changes from v1:
1. In-process kaswallet runtime (no external daemon) - Section 3
2. EVM-style Kaspa key UX with fallback behavior - Section 4

For complete implementation details, see companion document:
- **`igra-kaspa-integration-plan.md`** - Comprehensive implementation guide
  - Complete configuration reference (Section 1)
  - Lifecycle state machine (Section 8)
  - Error catalog with stable codes (Section 12)
  - Test scenario matrix (Section 17)
  - Phase-by-phase implementation plan (Section 18)

This document focuses on v2-specific changes only. Both documents must be
read together for complete implementation picture.
```

**Option B - Merge documents:**

Combine into single comprehensive spec with clear sections:
1. Architecture (from v2 doc)
2. Configuration (from integration plan)
3. Key Management (from v2 doc)
4. Lifecycle (from integration plan)
5. Implementation Plan (merged)

**Option C - Define separation of concerns:**

Add to both documents:
```markdown
## Document Structure

**Design Spec v2** (this document):
- System architecture and components
- Key management and signer UX
- High-level implementation phases

**Integration Plan**:
- Complete configuration reference
- Transaction lifecycle and state machine
- Error handling and recovery
- Detailed testing requirements
- Operational considerations

Both documents are authoritative in their respective domains.
Where conflicts exist, integration plan takes precedence for implementation details.
```

---

## Summary of Required Actions

### Before Implementation (CRITICAL):

1. ✅ **Fix Section 3.2:** Change to "Pin dependencies to commit SHA"
2. ✅ **Add Section 4.3.1:** Security warning about key reuse
3. ✅ **Fix Section 4.5:** Clarify derivation fallback semantics with examples
4. ✅ **Fix Section 6.2:** Align database naming with integration plan
5. ✅ **Add document header:** Cross-reference to integration plan and define relationship

### Before Phase 2 (MAJOR):

6. ✅ Complete config Section 2.2 or add explicit reference
7. ✅ Expand component specifications in Section 3.3
8. ✅ Add lifecycle states to Section 6.1 or reference integration plan
9. ✅ Add transaction type rejection list to Section 5.1
10. ✅ Add nonce gap handling section or reference integration plan

### Before Release (RECOMMENDED):

11. ✅ Reference complete error catalog
12. ✅ Clarify mining worker configuration
13. ✅ Add retry budget scope clarification
14. ✅ Complete network profile defaults
15. ✅ Add password caching details
16. ✅ Specify recovery timeout behavior
17. ✅ Align test matrix with integration plan
18. ✅ Align implementation phases
19. ✅ Define correlation ID format
20. ✅ Complete acceptance criteria

---

## Final Verdict

**Current State:** 6/10 - Good architectural vision, critical issues block implementation.

**After Critical Fixes:** 8/10 - Ready for implementation with minor gaps.

**After All Fixes:** 9/10 - Excellent companion to integration plan.

**Recommendation:**
1. Resolve 5 critical issues immediately (1-2 day effort)
2. Merge or clearly separate concerns from integration plan
3. Then proceed to implementation

**Key Insight:**
This document excels at high-level architecture (in-process runtime, key fallback UX)
but lacks rigor in implementation details. Either merge with integration plan or
establish clear cross-references and delineate responsibilities.

The **dependency pinning contradiction** must be fixed first - it's a showstopper
for deterministic builds.

---

## Appendix: Detailed Comparison Matrix

| Topic | Design Spec v2 | Integration Plan | Alignment |
|-------|----------------|------------------|-----------|
| **Dependency pinning** | Floating branches | Pin SHA | ❌ CONFLICT |
| **Database naming** | tx-map-v2-{net}.sqlite | tx-map-v1.sqlite | ❌ CONFLICT |
| **Config completeness** | 14 fields | 30+ fields | ⚠️ INCOMPLETE |
| **Lifecycle states** | Vague mention | 12 explicit states | ⚠️ INCOMPLETE |
| **Nonce gap handling** | Not mentioned | Full Section 4.4 | ❌ MISSING |
| **Transaction types** | Implicit | Explicit reject list | ⚠️ INCOMPLETE |
| **Error catalog** | 5 errors | 14+ stable codes | ⚠️ INCOMPLETE |
| **Test scenarios** | 10 tests | 18 scenarios | ⚠️ PARTIAL |
| **Security details** | Brief | More complete | ⚠️ INCOMPLETE |
| **Network profiles** | testnet-10 only | All 5 profiles | ⚠️ INCOMPLETE |
| **Key fallback** | ✅ Well defined | Not in integration plan | ✅ UNIQUE |
| **In-process runtime** | ✅ Well defined | Brief mention | ✅ UNIQUE |
| **Component arch** | Good overview | Not detailed | ✅ COMPLEMENTARY |

**Legend:**
- ✅ Good alignment or unique contribution
- ⚠️ Incomplete or needs reference to other doc
- ❌ Conflict or critical gap
