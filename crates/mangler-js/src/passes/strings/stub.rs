//! Renders the JS decoder-stub source for the string-obfuscation pass.
//!
//! The decoder is irreducibly hand-tuned JS (a base64 → `Uint8Array` → varint →
//! XOR-fold → UTF-8 pipeline whose every byte must mirror [`super::encode`]). Per
//! the rewrite plan, we build it as a single validated source fragment and let the
//! orchestrator parse it with swc (`Js::parse` / `Js::reparse`) and splice the
//! resulting statements — rather than `format!`-ing JS at codegen time with no
//! validation. A parse failure surfaces as a hard error, so a malformed stub can
//! never reach output.
//!
//! ## Layers ported (DEFAULT path)
//!
//! * **A** — DAG cross-ref + sharded base64 arrays (`partArrs[lutP[i]][lutS[i]]`).
//! * **B1** — split decode sink: K per-shard closures `__sh{k}` each owning a
//!   private `slot -> logical-index` table, plus a thin dispatcher exposed as
//!   `core` (and `core._` for the shard array).
//! * **B2** — runtime-derived base-key mask (`bk` reconstructed from `maskedB64`).
//! * **B4** — anti-tamper key coupling via a DJB2 hash of the integer tables.
//! * shims (`decoders`), decoys (`partitions + decoders`), dynamic key.
//!
//! ## Opt-in VM-coupled modes
//!
//! When [`StubParams::vm_decode`] is `Some`, `decodeOne(i)` is rendered as a thunk
//! into the bytecode interpreter ([`render_vm_decode_one`]) instead of the plain-JS
//! decoder; the caller splices the interpreter + program table above it. The
//! `self_coupled_key` / `exec_trace_key` fields add the source-coupled
//! (`SCK<digits>` sentinel + [`patch_self_coupled_expected`]) and execution-trace
//! ([`exec_trace_acc`]) integrity arms. All are inert when their fields are
//! off/zero, so the flag-off output is byte-identical to the plain decoder.

use super::encode::RUNTIME_KEY_LEN;

/// Parameters fully determining the rendered stub source.
pub struct StubParams {
    pub core_name: String,
    /// One vector per partition; each holds base64 strings.
    pub partitions: Vec<Vec<String>>,
    /// `lut_p[i]` = partition index of logical entry i.
    pub lut_p: Vec<u32>,
    /// `lut_s[i]` = slot index of logical entry i within its partition.
    pub lut_s: Vec<u32>,
    /// `refs[i]` = cross-ref logical index for entry i (`refs[0]` == 0 sentinel).
    pub refs: Vec<u32>,
    /// Base64 of `base_key XOR mask` (reconstructed at runtime).
    pub base_key_b64: String,
    /// Names of shim wrappers around `core_name`. Empty = call `core_name` directly.
    pub shim_names: Vec<String>,
    /// `shim_masks[0]` = XOR mask for shim 0; `shim_masks[1]` = ADD constant for shim 1.
    pub shim_masks: Vec<u32>,
    /// Involution permutation for shim 2 (length == N) when M >= 3.
    pub shim_perm: Option<Vec<u32>>,
    /// Decoy base64-shaped strings (frozen array). Empty disables the decoy block.
    pub decoys: Vec<String>,
    /// Anti-tamper poison byte (B4). `Some(b)` folds a DJB2-hash integrity delta
    /// into every key byte. `None` emits the plain decoder.
    pub tamper_byte: Option<u8>,
    /// Build-time DJB2 hash of `refs ++ lutP ++ lutS` (only used when `tamper_byte`).
    pub tamper_expected: u32,
    /// Runtime-bound key (opt-in). `Some(expr)` derives a keystream from the live
    /// JS expression and XOR-folds it into `bk`. `None` emits the plain `bk`.
    pub runtime_key_expr: Option<String>,
    /// Strings-in-VM (opt-in). `Some(_)` routes the per-index decode primitive
    /// through the bytecode VM: `decodeOne(i)` becomes a thunk call into the
    /// interpreter named here, and the caller is responsible for splicing the
    /// interpreter + program table ABOVE this stub. `None` emits the plain-JS
    /// decoder unchanged — output stays byte-identical to a non-VM build.
    pub vm_decode: Option<VmDecodeParams>,
    /// Stage 4 (opt-in): when `true` AND `vm_decode` is `Some`, the VM decode wrapper
    /// computes a runtime self-hash of the interpreter + decode-wrapper source and
    /// folds a poison delta into the in-VM key on mismatch. The build-time expected
    /// value is patched into the emitted output afterward by
    /// [`patch_self_coupled_expected`]. `false` (or no VM path) emits the wrapper
    /// unchanged.
    pub self_coupled_key: bool,
    /// Stage 4: the nonzero (odd) poison byte the self-coupled-key check ORs into the
    /// in-VM key on a source-hash mismatch. Ignored unless `self_coupled_key`.
    pub self_coupled_byte: u8,
    /// Stage 5 (`--exec-trace-key`, opt-in): the nonzero (odd) poison byte the in-VM
    /// execution-trace accumulator ORs into the key on divergence. `0` (off / non-VM /
    /// verify) makes the accumulator arm inert. Only emitted when `vm_decode` is `Some`.
    pub exec_trace_byte: u8,
    /// Stage 5: the build-time expected value of the in-VM execution-trace accumulator
    /// (see [`exec_trace_acc`]). Ignored unless `exec_trace_byte` is nonzero.
    pub exec_trace_expected: u32,
}

/// Metadata needed to render the VM-backed `decodeOne(i)` thunk call (strings-in-VM).
#[derive(Clone, Debug)]
pub struct VmDecodeParams {
    /// Interpreter function name (the `<interp>` in the thunk call).
    pub interp_name: String,
    /// Program-table `var` name (the `<table>` in the thunk call).
    pub table_name: String,
    /// Table index of the embedded decode chunk.
    pub chunk_index: usize,
    /// Free-global capture names, in `compiled.captures` order. Each must be a valid
    /// JS expression resolvable at the stub's scope (`atob`, `Uint8Array`, `String`,
    /// `Math`).
    pub captures: Vec<String>,
    /// Slot index where captures begin (thunk arg `capStart`).
    pub cap_start: u32,
    /// Positional param count (thunk arg `pcount`).
    pub pcount: u32,
}

/// Comma-join a `u32` slice into a JS array body (`1,2,3`).
fn nums(v: &[u32]) -> String {
    v.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

/// Comma-join a string slice into a JS array body of double-quoted base64 literals
/// (inputs are base64, no escaping needed).
fn strs(v: &[String]) -> String {
    v.iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(",")
}

/// The per-index decode primitive, authored as a FLAT, VM-eligible function
/// expression (helpers inlined, explicit cursors — NO `++`/`--` in sub-expressions).
///
/// SINGLE SOURCE OF TRUTH for the VM-backed decode path. Byte-faithful to the
/// plain-JS `render_get_raw`/`render_read_varint`/`render_decode_one`/
/// `render_utf8_decode` semantics: base64 → `Uint8Array`, a `[prefix_len]` byte, a
/// LEB128 varint payload length, an XOR fold against `bk` (and `bk ^ ref-bytes` for
/// cross-ref entries `i > 0`), and a manual UTF-8 decode.
///
/// Params, positional, in the order the JS thunk threads them:
///   `(i, maskedB64, partArrs, lutP, lutS, refs, N, expectedHash, tamperByte,
///     probeDelta, keySrc, keyOn, selfDelta)`
///
/// Param 2 is the base64 string `maskedB64` (`base_key XOR mask`); `bk` is
/// reconstructed IN-VM by base64-decoding it and folding `mask` back out over the
/// live `refs`/`lutP`/`lutS` tables (mirror of `derive_key_mask` + `mask_base_key`).
/// `keySrc` is the optional runtime-key SOURCE STRING; when non-empty (`keyOn`) the
/// 17-byte keystream is derived IN-VM and XOR-folded into `bk`. The anti-tamper `_td`
/// fold is computed IN-VM: a DJB2 hash of `refs.concat(lutP).concat(lutS)` is compared
/// to `expectedHash`; a mismatch ORs `tamperByte` into `_td`, alongside `probeDelta`
/// and `selfDelta`. `_td` is XORed into every key byte. With tamper off the wrapper
/// passes the correct hash + zero deltas → `_td` is provably 0 → byte-identical decode.
pub const VM_DECODE_PRIMITIVE: &str = r#"function(i, maskedB64, partArrs, lutP, lutS, refs, N, expectedHash, tamperByte, probeDelta, keySrc, keyOn, selfDelta) {
  var _ms = atob(maskedB64);
  var bk = new Uint8Array(_ms.length);
  var _bi = 0;
  while (_bi < _ms.length) { bk[_bi] = _ms.charCodeAt(_bi) & 255; _bi = _bi + 1; }
  var _st = refs.concat(lutP).concat(lutS);
  var _m = 0;
  while (_m < bk.length) {
    var _acc = (_m * 158 + 1) & 255, _p = 0;
    while (_p < _st.length) { _acc = (_acc + ((_st[_p] + _p + _m) & 255)) & 255; _p = _p + 1; }
    bk[_m] = (bk[_m] ^ _acc) & 255;
    _m = _m + 1;
  }
  if (keyOn) {
    var _rk = new Uint8Array(17);
    var _L = 0;
    while (_L < 17) {
      var _h = (2166136261 + Math.imul(_L, 2654435761)) >>> 0;
      var _pp = 0;
      while (_pp < keySrc.length) {
        var _cc = (keySrc.charCodeAt(_pp) + _pp + _L) & 65535;
        _h = (_h ^ _cc) >>> 0;
        _h = Math.imul(_h, 16777619) >>> 0;
        _h = (_h ^ (_h >>> 13)) >>> 0;
        _pp = _pp + 1;
      }
      _rk[_L] = _h & 255;
      _L = _L + 1;
    }
    var _rq = 0;
    while (_rq < bk.length) { bk[_rq] = (bk[_rq] ^ _rk[_rq % _rk.length]) & 255; _rq = _rq + 1; }
  }
  var _ts = refs.concat(lutP).concat(lutS), _th = 5381, _ti = 0;
  while (_ti < _ts.length) { _th = (_th * 33 + _ts[_ti]) >>> 0; _ti = _ti + 1; }
  var _td = ((_th !== expectedHash ? tamperByte : 0) | probeDelta | selfDelta) & 255;
  var s0 = atob(partArrs[lutP[i]][lutS[i]]);
  var self_ = new Uint8Array(s0.length);
  for (var a = 0; a < s0.length; a++) { self_[a] = s0.charCodeAt(a); }
  var pl = self_[0];
  var cursor = 1 + pl;
  var val = 0, mult = 1, b, c = 0;
  do { b = self_[cursor + c]; val += (b & 0x7F) * mult; mult *= 128; c++; } while (b & 0x80);
  var payloadLen = val, payloadStart = cursor + c;
  var out = new Uint8Array(payloadLen);
  if (i === 0) {
    for (var j = 0; j < payloadLen; j++) {
      out[j] = self_[payloadStart + j] ^ (bk[j % bk.length] ^ _td);
    }
  } else {
    var ri = refs[i];
    var rs = atob(partArrs[lutP[ri]][lutS[ri]]);
    var ref_ = new Uint8Array(rs.length);
    for (var d = 0; d < rs.length; d++) { ref_[d] = rs.charCodeAt(d); }
    var rl = ref_.length;
    for (var k = 0; k < payloadLen; k++) {
      out[k] = self_[payloadStart + k] ^ ((bk[k % bk.length] ^ _td) ^ ref_[k % rl]);
    }
  }
  var res = '', p = 0;
  while (p < out.length) {
    var ch = out[p]; p = p + 1;
    if (ch < 0x80) { res += String.fromCharCode(ch); }
    else if (ch < 0xE0) {
      var b1 = out[p]; p = p + 1;
      res += String.fromCharCode(((ch & 0x1F) << 6) | (b1 & 0x3F));
    }
    else if (ch < 0xF0) {
      var c2 = out[p]; p = p + 1;
      var c3 = out[p]; p = p + 1;
      res += String.fromCharCode(((ch & 0x0F) << 12) | ((c2 & 0x3F) << 6) | (c3 & 0x3F));
    } else {
      var d2 = out[p]; p = p + 1;
      var d3 = out[p]; p = p + 1;
      var d4 = out[p]; p = p + 1;
      var cp = ((ch & 0x07) << 18) | ((d2 & 0x3F) << 12) | ((d3 & 0x3F) << 6) | (d4 & 0x3F);
      cp -= 0x10000;
      res += String.fromCharCode(0xD800 | (cp >> 10), 0xDC00 | (cp & 0x3FF));
    }
  }
  return res;
}"#;

/// Stage 5 (`--exec-trace-key`, opt-in): the execution-trace-augmented variant of
/// [`VM_DECODE_PRIMITIVE`]. Identical to the base primitive EXCEPT it (1) appends two
/// trailing positional params `traceExpected, traceByte`, and (2) folds a rolling u32
/// accumulator `_xa` over the reconstructed key `bk`, the integrity-table stream
/// `_st`, and the `keyOn` branch outcome, then ORs `_xd = (_xa !== traceExpected ?
/// traceByte : 0)` into `_td`. See [`exec_trace_acc`] for the build-time mirror.
///
/// Built by splicing into the base constant so the two paths cannot drift. Embedded
/// ONLY when the flag is on; the flag-off path embeds the unmodified primitive, so
/// strings-in-VM output stays byte-identical to a build without Stage 5.
pub fn vm_decode_primitive_with_trace() -> String {
    let with_params =
        VM_DECODE_PRIMITIVE.replacen("selfDelta) {", "selfDelta, traceExpected, traceByte) {", 1);
    let acc = "  var _xa = (2166136261 ^ (keyOn ? 1 : 0)) >>> 0;\n  \
        var _xi = 0;\n  \
        while (_xi < bk.length) {\n    \
        _xa = (_xa ^ (bk[_xi] & 255)) >>> 0;\n    \
        _xa = Math.imul(_xa, 16777619) >>> 0;\n    \
        _xa = (_xa + ((_xi * 2654435761) >>> 0)) >>> 0;\n    \
        _xi = _xi + 1;\n  }\n  \
        var _xj = 0;\n  \
        while (_xj < _st.length) {\n    \
        _xa = (_xa ^ ((_st[_xj] + _xj) & 65535)) >>> 0;\n    \
        _xa = Math.imul(_xa, 16777619) >>> 0;\n    \
        _xa = (_xa ^ (_xa >>> 13)) >>> 0;\n    \
        _xj = _xj + 1;\n  }\n  \
        var _xd = ((_xa >>> 0) !== traceExpected ? traceByte : 0) & 255;\n  ";
    with_params.replacen(
        "  var _td = ((_th !== expectedHash ? tamperByte : 0) | probeDelta | selfDelta) & 255;",
        &format!(
            "{acc}var _td = ((_th !== expectedHash ? tamperByte : 0) | probeDelta | selfDelta | _xd) & 255;"
        ),
        1,
    )
}

/// Stage 5 (`--exec-trace-key`, opt-in) — the build-time mirror of the in-VM
/// execution-trace integrity accumulator. Folds the reconstructed key bytes `bk`, the
/// integrity-table stream `refs ++ lutP ++ lutS`, and the `key_on` branch outcome
/// through the SAME cross-engine-deterministic u32 ops (`Math.imul`, `>>> 0`, `^`,
/// `+`, `& 0xFF`, `& 0xFFFF`, `>> 13`) the bytecode executes. A faithful run on any
/// conforming engine reproduces this exact value; an instrumented/altered VM does not.
pub fn exec_trace_acc(bk: &[u8], refs: &[u32], lut_p: &[u32], lut_s: &[u32], key_on: bool) -> u32 {
    let mut xa: u32 = 2166136261u32 ^ (if key_on { 1 } else { 0 });
    for (i, &b) in bk.iter().enumerate() {
        xa ^= (b as u32) & 0xFF;
        xa = xa.wrapping_mul(16777619);
        xa = xa.wrapping_add((i as u32).wrapping_mul(2654435761));
    }
    let st = refs.iter().chain(lut_p.iter()).chain(lut_s.iter());
    for (j, &x) in st.enumerate() {
        xa ^= x.wrapping_add(j as u32) & 0xFFFF;
        xa = xa.wrapping_mul(16777619);
        xa ^= xa >> 13;
    }
    xa
}

/// The `slot -> logical-index` table for shard `k` (inverse of the `(lut_p, lut_s)`
/// placement): for each logical entry assigned to partition `k`, `out[lut_s[i]] = i`.
fn slot_to_logical_for(params: &StubParams, k: usize) -> Vec<u32> {
    let len = params.partitions.get(k).map(Vec::len).unwrap_or(0);
    let mut out = vec![0u32; len];
    for (i, (&p, &s)) in params.lut_p.iter().zip(params.lut_s.iter()).enumerate() {
        if p as usize == k {
            out[s as usize] = i as u32;
        }
    }
    out
}

/// Render the `var bk = …` reconstruction (B2). The base64 literal holds
/// `base_key XOR mask`; this IIFE base64-decodes it, recomputes `mask` at runtime
/// by folding over the live `refs`/`lutP`/`lutS` tables (byte-exact mirror of
/// `derive_key_mask`), and XORs it back out. When `runtime_key_expr` is `Some`, a
/// second IIFE derives a keystream from the live host value and folds it in.
fn render_bk_decl(masked_b64: &str, runtime_key_expr: Option<&str>) -> String {
    let base = format!(
        "var bk = (function(s){{\
var b=atob(s);var u=new Uint8Array(b.length);\
for(var k=0;k<b.length;k++)u[k]=b.charCodeAt(k);\
var st=refs.concat(lutP).concat(lutS);\
for(var m=0;m<u.length;m++){{\
var acc=(m*158+1)&255;\
for(var p=0;p<st.length;p++){{acc=(acc+((st[p]+p+m)&255))&255;}}\
u[m]=(u[m]^acc)&255;\
}}\
return u;}})(\"{masked_b64}\");"
    );
    match runtime_key_expr {
        None => base,
        Some(expr) => format!(
            "{base}\
var rk = (function(){{var s;try{{s=\"\"+({expr});}}catch(e){{s=\"\";}}\
var r=new Uint8Array({len});\
for(var L=0;L<{len};L++){{\
var h=(2166136261+Math.imul(L,2654435761))>>>0;\
for(var p=0;p<s.length;p++){{\
var c=(s.charCodeAt(p)+p+L)&65535;\
h=(h^c)>>>0;h=Math.imul(h,16777619)>>>0;h=(h^(h>>>13))>>>0;\
}}\
r[L]=h&255;\
}}\
return r;}})();\
for(var _q=0;_q<bk.length;_q++)bk[_q]=(bk[_q]^rk[_q%rk.length])&255;",
            len = RUNTIME_KEY_LEN,
        ),
    }
}

/// `getRaw(i)`: base64-decode entry `i` into a memoised `Uint8Array`.
fn render_get_raw() -> &'static str {
    "function getRaw(i){\n\
  if(rawCache[i]) return rawCache[i];\n\
  var s = atob(partArrs[lutP[i]][lutS[i]]);\n\
  var u = new Uint8Array(s.length);\n\
  for(var k=0;k<s.length;k++) u[k] = s.charCodeAt(k);\n\
  rawCache[i] = u;\n\
  return u;\n\
}"
}

/// `readVarint(buf, off)`: LEB128 decode returning `[value, bytes_consumed]`.
fn render_read_varint() -> &'static str {
    "function readVarint(buf, off){\n\
  var val = 0, mult = 1, b, c = 0;\n\
  do { b = buf[off+c]; val += (b & 0x7F) * mult; mult *= 128; c++; } while (b & 0x80);\n\
  return [val, c];\n\
}"
}

/// `utf8Decode(u)`: manual UTF-8 → JS string (incl. astral surrogate pairs).
fn render_utf8_decode() -> &'static str {
    "function utf8Decode(u){\n\
  var out = '', i = 0;\n\
  while (i < u.length) {\n\
    var c = u[i++];\n\
    if (c < 0x80) { out += String.fromCharCode(c); }\n\
    else if (c < 0xE0) { out += String.fromCharCode(((c & 0x1F) << 6) | (u[i++] & 0x3F)); }\n\
    else if (c < 0xF0) {\n\
      var c2 = u[i++], c3 = u[i++];\n\
      out += String.fromCharCode(((c & 0x0F) << 12) | ((c2 & 0x3F) << 6) | (c3 & 0x3F));\n\
    } else {\n\
      var d2 = u[i++], d3 = u[i++], d4 = u[i++];\n\
      var cp = ((c & 0x07) << 18) | ((d2 & 0x3F) << 12) | ((d3 & 0x3F) << 6) | (d4 & 0x3F);\n\
      cp -= 0x10000;\n\
      out += String.fromCharCode(0xD800 | (cp >> 10), 0xDC00 | (cp & 0x3FF));\n\
    }\n\
  }\n\
  return out;\n\
}"
}

/// `decodeOne(i)`: parse the wire header for logical index `i`, XOR-decode the
/// payload against `bk` (and the cross-ref entry for `i > 0`), and UTF-8 decode.
/// `bk_expr` is the per-byte key expression (folds `_td` when tamper is on).
fn render_decode_one(bk_expr: &str) -> String {
    format!(
        "function decodeOne(i){{\n\
  var self_ = getRaw(i);\n\
  var pl = self_[0];\n\
  var cursor = 1 + pl;\n\
  var rv = readVarint(self_, cursor);\n\
  var payloadLen = rv[0], payloadStart = cursor + rv[1];\n\
  var out = new Uint8Array(payloadLen);\n\
  if (i === 0) {{\n\
    for (var j = 0; j < payloadLen; j++) {{\n\
      out[j] = self_[payloadStart + j] ^ {bk_expr};\n\
    }}\n\
  }} else {{\n\
    var ref_ = getRaw(refs[i]);\n\
    var rl = ref_.length;\n\
    for (var j = 0; j < payloadLen; j++) {{\n\
      out[j] = self_[payloadStart + j] ^ ({bk_expr} ^ ref_[j % rl]);\n\
    }}\n\
  }}\n\
  return utf8Decode(out);\n\
}}"
    )
}

/// Strings-in-VM: render the `decodeOne(i)` wrapper that thunks the per-index decode
/// into the bytecode interpreter over the embedded decode chunk. `bk` is reconstructed
/// IN-VM from the `masked_b64` string; the optional runtime keystream is derived in-VM
/// from the host value string. The `_td` tamper fold is computed in-VM (the data-hash
/// check against `expected`, plus the beautify probe threaded in as `probeDelta`).
///
/// Thunk shape mirrors the interpreter signature `interp(code, consts, args, caps,
/// capStart, pcount, receiver)`: the positional decode params are the `args` array,
/// `[caps]` the captures, then `capStart`, `pcount`, `this`.
#[allow(clippy::too_many_arguments)]
fn render_vm_decode_one(
    vm: &VmDecodeParams,
    tamper: bool,
    expected: u32,
    tamper_byte: u8,
    masked_b64: &str,
    runtime_key_expr: Option<&str>,
    self_coupled: bool,
    self_byte: u8,
    exec_trace_byte: u8,
    exec_trace_expected: u32,
) -> String {
    let caps = vm.captures.join(",");
    let interp = &vm.interp_name;
    let table = &vm.table_name;
    let k = vm.chunk_index;
    let cap_start = vm.cap_start;
    let pcount = vm.pcount;

    // Beautify probe: when tamper is on, stringify `decodeOne` itself; a beautified
    // body adds newlines, raising `_pd` to the tamper byte (ORs into the in-VM `_td`).
    let (probe_decl, probe_arg) = if tamper {
        (
            format!(
                "  var _pd=0; try{{ if(((\"\"+decodeOne).split(\"\\n\").length-1)>=3) _pd={tamper_byte}; }}catch(e){{}}\n"
            ),
            "_pd",
        )
    } else {
        (String::new(), "0")
    };
    let tb = if tamper { tamper_byte } else { 0 };

    // The runtime keystream is derived IN-VM from a SOURCE STRING (`keySrc`). `keyOn`
    // (1/0) signals whether a runtime key is CONFIGURED — the in-VM derivation runs on
    // that flag, not on `keySrc.length`, so a legitimately-empty live value still
    // cancels the build-time fold.
    let (key_decl, key_arg, key_on) = match runtime_key_expr {
        Some(expr) => (
            format!("  var _ks=\"\";try{{_ks=\"\"+({expr});}}catch(e){{}}\n"),
            "_ks".to_string(),
            "1",
        ),
        None => (String::new(), "\"\"".to_string(), "0"),
    };

    // Stage 4 (self-coupled key): compute a runtime hash of the interpreter's AND this
    // decode wrapper's own source and raise `selfDelta` to the tamper byte on mismatch
    // with the build-time expected value baked into a fixed-width `SCK<10 digits>`
    // sentinel (patched after codegen). The hash NORMALIZES `decodeOne`'s own source by
    // rewriting every `SCK<10 digits>` run to the canon sentinel, so the self-reference
    // is a clean fixpoint. Off: pass `0` so the arm is inert.
    let (self_decl, self_arg) = if self_coupled {
        (
            format!(
                "  var _scd=0;\n  try{{\n    var _se=\"{slot}\";\n    var _dj=function(s){{var h=5381,k=0;for(;k<s.length;k++)h=(h*33+s.charCodeAt(k))>>>0;return h;}};\n    var _norm=(\"\"+decodeOne).replace(/SCK[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]/g,\"{canon}\");\n    var _sh=(_dj(\"\"+{interp})^_dj(_norm))>>>0;\n    if(_sh!==((+_se.slice(3))>>>0)) _scd={self_byte};\n  }}catch(e){{}}\n",
                slot = SELF_COUPLED_SLOT,
                canon = SELF_COUPLED_CANON,
            ),
            "_scd".to_string(),
        )
    } else {
        (String::new(), "0".to_string())
    };

    // Stage 5: thread the execution-trace expected value + poison byte as TWO EXTRA
    // trailing positional args, ONLY when the flag is on (the augmented primitive with
    // matching arity is embedded under the same gate). Off: omit them entirely.
    let trace_args = if exec_trace_byte != 0 {
        format!(", {exec_trace_expected}, {exec_trace_byte}")
    } else {
        String::new()
    };
    let args = format!(
        "[i, \"{masked_b64}\", partArrs, lutP, lutS, refs, N, {expected}, {tb}, {probe_arg}, {key_arg}, {key_on}, {self_arg}{trace_args}], [{caps}], {cap_start}, {pcount}, this"
    );
    let call_body = format!("  return {interp}({table}[{k}][0], {table}[{k}][1], {args});\n");
    format!("function decodeOne(i){{\n{probe_decl}{key_decl}{self_decl}{call_body}}}")
}

/// Stage 4 self-coupled-key sentinels. Both are fixed-width `SCK`-prefixed tokens with
/// a 10-digit numeric tail (a u32 max is `4294967295`, exactly 10 digits), so the token
/// length is INVARIANT across the placeholder and patched forms — patching never shifts
/// byte offsets, which makes the self-referential hash a clean fixpoint.
///
/// * [`SELF_COUPLED_SLOT`] is the `_se` value slot the post-codegen patch rewrites with
///   the real expected hash. Its `9999999999` tail makes it textually unique.
/// * [`SELF_COUPLED_CANON`] is the normalization target both the build and runtime fold
///   every `SCK<10 digits>` run down to before hashing.
pub const SELF_COUPLED_SLOT: &str = "SCK9999999999";
pub const SELF_COUPLED_CANON: &str = "SCK0000000000";

/// Rewrite every `SCK<10 digits>` run in `s` to [`SELF_COUPLED_CANON`], the exact
/// mirror of the runtime `.replace(/SCK[0-9]{10}/g, "SCK0000000000")`.
fn normalize_self_coupled(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'S'
            && i + 13 <= bytes.len()
            && &bytes[i..i + 3] == b"SCK"
            && bytes[i + 3..i + 13].iter().all(u8::is_ascii_digit)
        {
            out.push_str(SELF_COUPLED_CANON);
            i += 13;
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Build the patched slot string for `expected` (a u32), zero-padded to 10 digits.
fn self_coupled_token(expected: u32) -> String {
    format!("SCK{expected:010}")
}

/// Given `output` and a byte offset at the `f` of a `function` keyword, return the byte
/// range covering the whole `function NAME(args){BODY}` exactly as
/// `Function.prototype.toString` would report it. Tracks `'`/`"` string literals (with
/// `\` escapes) so braces inside strings are ignored. The emitted interpreter/decoder
/// use no template literals and no brace-bearing regex, so string-awareness suffices.
fn function_span(output: &str, decl_start: usize) -> Option<std::ops::Range<usize>> {
    let bytes = output.as_bytes();
    let mut i = decl_start;
    while i < bytes.len() && bytes[i] != b'{' {
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            i = skip_string(bytes, i)?;
            continue;
        }
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                i = skip_string(bytes, i)?;
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(decl_start..i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Skip a `'`/`"`-delimited string starting at `bytes[start]` (the opening quote),
/// honoring `\` escapes. Returns the index just past the closing quote.
fn skip_string(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            c if c == quote => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Stage 4 post-codegen patch: bind the decode key to the interpreter + decode-wrapper
/// source. `output` is the FINAL emitted JS (post minify/codegen, post anti-tamper
/// wrap). `interp_name` is the strings-VM interpreter name AS GENERATED by the strings
/// pass.
///
/// Computes `expected = djb2(""+interp) ^ djb2(normalize(""+decodeOne))` over the EXACT
/// emitted source — the byte-faithful mirror of the runtime fold — then rewrites the
/// unique [`SELF_COUPLED_SLOT`] token. Missing source or hash metadata is a
/// transformation error; an unpatched protection artifact is never returned.
///
/// The interpreter + decode wrapper live INSIDE the protected `core` IIFE, so codegen
/// may have RENAMED the interpreter local. We therefore locate the decode wrapper by
/// the unique slot token first, then recover the interpreter's actual emitted name from
/// the wrapper's own `return <interp>(<table>[k]…)` thunk call — falling back to the
/// generated `interp_name` if that fails. Both spans are read from the FINAL output, so
/// the hashes match the runtime `""+interp` / `""+decodeOne` exactly regardless of how
/// the locals were renamed.
pub fn patch_self_coupled_expected(
    output: String,
    interp_name: &str,
) -> mangler_core::Result<String> {
    use mangler_core::{Error, hash::djb2_utf16};

    // The decode wrapper: the SMALLEST function span containing the unique slot token.
    let Some(slot_pos) = output.find(SELF_COUPLED_SLOT) else {
        return Err(Error::transform(
            "strings",
            "Self-coupled decoder hash slot is missing",
        ));
    };
    let mut decode_range: Option<std::ops::Range<usize>> = None;
    let mut search = 0usize;
    while let Some(off) = output[search..].find("function") {
        let fstart = search + off;
        if let Some(r) = function_span(&output, fstart)
            && r.start <= slot_pos
            && slot_pos < r.end
        {
            let better = match &decode_range {
                None => true,
                Some(cur) => (r.end - r.start) < (cur.end - cur.start),
            };
            if better {
                decode_range = Some(r);
            }
        }
        search = fstart + "function".len();
    }
    let Some(decode_range) = decode_range else {
        return Err(Error::transform(
            "strings",
            "Self-coupled decoder function could not be located",
        ));
    };
    let wrapper_src = &output[decode_range.clone()];

    // Recover the interpreter's emitted name from the wrapper's thunk call
    // `return <interp>(<table>[…` — robust to the interpreter local being renamed —
    // and fall back to the generated name.
    let resolved_interp = interp_callee_in_wrapper(wrapper_src).unwrap_or(interp_name);
    let Some(interp_range) =
        super::source_function::resolve(&output, resolved_interp, decode_range.clone())
    else {
        return Err(Error::transform(
            "strings",
            "Self-coupled interpreter source could not be resolved",
        ));
    };
    let interp_src = &output[interp_range.clone()];

    let decode_src = normalize_self_coupled(&output[decode_range]);
    let expected = djb2_utf16(interp_src) ^ djb2_utf16(&decode_src);
    Ok(output.replacen(SELF_COUPLED_SLOT, &self_coupled_token(expected), 1))
}

/// Recover the interpreter callee name from a decode-wrapper source by finding its
/// thunk call `<interp>(<table>[…][0]…)`: the identifier that precedes a `(` which is
/// immediately followed by another identifier indexed by `[`. Returns the identifier
/// slice, or `None` if the shape is not found.
fn interp_callee_in_wrapper(wrapper: &str) -> Option<&str> {
    let bytes = wrapper.as_bytes();
    let is_id = |c: u8| c == b'_' || c == b'$' || c.is_ascii_alphanumeric();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            // The callee identifier ends just before this `(`.
            let mut s = i;
            while s > 0 && is_id(bytes[s - 1]) {
                s -= 1;
            }
            // Must be a real identifier (not empty, not starting with a digit).
            if s < i && !bytes[s].is_ascii_digit() {
                // The first argument must be `<ident>[` (the program-table index) —
                // the distinguishing shape of the thunk call.
                let mut a = i + 1;
                let astart = a;
                while a < bytes.len() && is_id(bytes[a]) {
                    a += 1;
                }
                if a > astart && a < bytes.len() && bytes[a] == b'[' {
                    return Some(&wrapper[s..i]);
                }
            }
        }
        i += 1;
    }
    None
}

/// Render the `var <core> = (function(){ … })();` decode-by-index IIFE.
fn render_core(params: &StubParams, n: usize) -> String {
    let part_decls = params
        .partitions
        .iter()
        .enumerate()
        .map(|(k, items)| format!("var __p{k}=[{}];", strs(items)))
        .collect::<Vec<_>>()
        .join("\n");

    let part_arrs_list = (0..params.partitions.len())
        .map(|k| format!("__p{k}"))
        .collect::<Vec<_>>()
        .join(",");

    let decoy_block = if params.decoys.is_empty() {
        String::new()
    } else {
        format!(
            "try{{Object.freeze([{}]);}}catch(e){{}}\n",
            strs(&params.decoys)
        )
    };

    // Anti-tamper (B4): when on, fold a DJB2-hash integrity delta `_td` into every
    // key byte. An untampered run computes the exact expected hash → `_td` stays 0
    // → byte-identical decode. The expected hash is computed in Rust by `djb2_nums`.
    //
    // VM path: FU2 Stage A moves the ENTIRE `_td` computation into the bytecode
    // primitive (the DJB2 data-hash check runs in-VM, the beautify probe is threaded
    // in as `probeDelta` by `render_vm_decode_one`). So NO JS `_td` var and no
    // `bk_expr` are emitted on the VM path.
    let (delta_decl, bk_expr) = match (params.tamper_byte, params.vm_decode.is_some()) {
        (Some(b), false) => (
            format!(
                "var _td=0;\
                 try{{var _ts=refs.concat(lutP).concat(lutS),_th=5381;\
                 for(var _ti=0;_ti<_ts.length;_ti++)_th=(_th*33+_ts[_ti])>>>0;\
                 if(_th!=={expected})_td={b};}}catch(e){{}}\n",
                expected = params.tamper_expected,
            ),
            "(bk[j % bk.length] ^ _td)".to_string(),
        ),
        (Some(_b), true) => (String::new(), String::new()),
        (None, _) => (String::new(), "bk[j % bk.length]".to_string()),
    };

    // `decodeOne(i)` is the shared per-logical-index decode primitive. On the plain
    // path it is authored JS (helpers + the XOR/UTF-8 loop). When `vm_decode` is set,
    // it instead thunks into the bytecode interpreter over the embedded decode chunk;
    // the helpers collapse away (their logic is now bytecode).
    let (helpers, decode_one) = match &params.vm_decode {
        None => {
            let helpers = format!(
                "{}\n{}\n{}\n",
                render_get_raw(),
                render_read_varint(),
                render_utf8_decode()
            );
            (helpers, render_decode_one(&bk_expr))
        }
        Some(vm) => (
            String::new(),
            render_vm_decode_one(
                vm,
                params.tamper_byte.is_some(),
                params.tamper_expected,
                params.tamper_byte.unwrap_or(0),
                &params.base_key_b64,
                params.runtime_key_expr.as_deref(),
                params.self_coupled_key,
                params.self_coupled_byte,
                params.exec_trace_byte,
                params.exec_trace_expected,
            ),
        ),
    };

    // B1: split decode sink across K per-shard closures.
    let shard_count = params.partitions.len().max(1);
    let mut shard_decls = String::new();
    for k in 0..shard_count {
        let slot_to_logical = slot_to_logical_for(params, k);
        shard_decls.push_str(&format!(
            "var __sl{k} = [{sl}];\n\
var __c{k} = {{}};\n\
function __sh{k}(slot){{\n\
  if (__c{k}[slot] !== undefined) return __c{k}[slot];\n\
  var i = __sl{k}[slot];\n\
  if (i === undefined) return undefined;\n\
  var __v = decodeOne(i);\n\
  __c{k}[slot] = __v;\n\
  return __c{k}[slot];\n\
}}\n",
            k = k,
            sl = nums(&slot_to_logical),
        ));
    }
    let shards_list = (0..shard_count)
        .map(|k| format!("__sh{k}"))
        .collect::<Vec<_>>()
        .join(",");

    // VM path: both the `bk` base64+mask reconstruction AND the `rk` keystream
    // derivation move INTO the bytecode primitive, so the JS IIFE emits nothing.
    let bk_decl = if params.vm_decode.is_some() {
        String::new()
    } else {
        render_bk_decl(&params.base_key_b64, params.runtime_key_expr.as_deref())
    };

    format!(
        "var {core_name} = (function(){{\n\
{part_decls}\n\
{decoy_block}var partArrs = [{part_arrs_list}];\n\
var lutP = [{lutp}];\n\
var lutS = [{luts}];\n\
var refs = [{refs}];\n\
var N = {n};\n\
{bk_decl}\n\
var rawCache = new Array(N);\n\
{helpers}{delta_decl}{decode_one}\n\
{shard_decls}var __shards = [{shards_list}];\n\
var __disp = function(i){{ return __shards[lutP[i]](lutS[i]); }};\n\
__disp._ = __shards;\n\
return __disp;\n\
}})();\n",
        core_name = params.core_name,
        lutp = nums(&params.lut_p),
        luts = nums(&params.lut_s),
        refs = nums(&params.refs),
    )
}

/// Render the full decoder stub: the `core` IIFE followed by the M shim wrappers.
pub fn render_stub(params: &StubParams) -> String {
    let n = params.refs.len();
    let core_body = render_core(params, n);

    let mut shim_decls = String::new();
    for (idx, name) in params.shim_names.iter().enumerate() {
        match idx {
            0 => {
                let mask = params.shim_masks.first().copied().unwrap_or(0);
                shim_decls.push_str(&format!(
                    "var {name} = function(i){{ return {core}((i ^ {mask}) >>> 0); }};\n",
                    core = params.core_name,
                ));
            }
            1 => {
                let add = params.shim_masks.get(1).copied().unwrap_or(0);
                shim_decls.push_str(&format!(
                    "var {name} = function(i){{ return {core}((((i - {add}) % {n}) + {n}) % {n}); }};\n",
                    core = params.core_name,
                    n = n.max(1),
                ));
            }
            2 => {
                let perm = params
                    .shim_perm
                    .as_ref()
                    .expect("shim_perm required for shim 2");
                shim_decls.push_str(&format!("var {name}_perm = [{p}];\n", p = nums(perm)));
                shim_decls.push_str(&format!(
                    "var {name} = function(i){{ return {core}({name}_perm[i]); }};\n",
                    core = params.core_name,
                ));
            }
            _ => {}
        }
    }

    format!("{core_body}{shim_decls}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_jsast::{Js, ParseOpts};

    fn parses(src: &str) -> bool {
        Js::reparse(src, &ParseOpts::default()).is_ok()
    }

    fn base_params() -> StubParams {
        StubParams {
            core_name: "__core".into(),
            partitions: vec![vec!["AAEC".into()], vec!["AwQF".into()]],
            lut_p: vec![0, 1],
            lut_s: vec![0, 0],
            refs: vec![0, 0],
            base_key_b64: "AAAAAAAAAAAAAAAAAAAAAA==".into(),
            shim_names: vec![],
            shim_masks: vec![],
            shim_perm: None,
            decoys: vec![],
            tamper_byte: None,
            tamper_expected: 0,
            runtime_key_expr: None,
            vm_decode: None,
            self_coupled_key: false,
            self_coupled_byte: 0,
            exec_trace_byte: 0,
            exec_trace_expected: 0,
        }
    }

    #[test]
    fn optimized_raw_decoder_keeps_distinct_byte_and_text_buffers() {
        use mangler_core::Language;
        let mut params = base_params();
        // One real encoded "next" entry. Use the raw generated decoder here so
        // intrinsic isolation cannot accidentally conceal inlining collisions.
        params.partitions = vec![vec!["BWh2mYlIBF1DNHgLPHCmHm1Ac/bA/OY=".into()]];
        params.lut_p = vec![0];
        params.lut_s = vec![0];
        params.refs = vec![0];
        params.base_key_b64 = "N4MK67+TFfItFH+a91Rf0R4=".into();
        let source = format!("{}globalThis.__out=__core(0);", render_stub(&params));
        let expected = "globalThis.__out='next';";
        mangler_testkit::assert_behaviorally_equal(expected, &source);
        let output = Js::with_globals(|| {
            let mut ast = Js.parse(&source, &ParseOpts::default()).unwrap();
            let marks = Js::resolve(&mut ast);
            Js::print_optimized(ast, marks, false, &[])
        });
        mangler_testkit::assert_behaviorally_equal(expected, &output);
    }

    #[test]
    fn renders_valid_js_for_default_params() {
        let s = render_stub(&base_params());
        assert!(s.contains("var __core"));
        assert!(s.contains("__p0") && s.contains("__p1"));
        assert!(parses(&s), "stub must parse:\n{s}");
    }

    #[test]
    fn renders_valid_js_with_all_features() {
        let mut p = base_params();
        p.shim_names = vec!["_s0".into(), "_s1".into(), "_s2".into()];
        p.shim_masks = vec![123, 1];
        p.shim_perm = Some(vec![1, 0]);
        p.decoys = vec!["ZGVjb3k=".into()];
        p.tamper_byte = Some(0x33);
        p.tamper_expected = 999;
        p.runtime_key_expr = Some("location.host".into());
        let s = render_stub(&p);
        assert!(parses(&s), "feature-rich stub must parse:\n{s}");
        assert!(s.contains("Object.freeze"));
        assert!(s.contains("_td"));
        assert!(s.contains("Math.imul"));
    }
}
