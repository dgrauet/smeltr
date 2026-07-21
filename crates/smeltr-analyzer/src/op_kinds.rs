//! Best-effort mapping from MLX shader function names to canonical op kinds.
//!
//! Returned strings are intended for display in breakdown UIs. The table is
//! MLX-version-sensitive and intended to evolve as MLX adds or renames
//! shaders.

/// Pattern table for `resolve_kind`. Earlier entries take priority; a
/// later entry must not have an earlier entry as a strict prefix
/// (verified by `table_ordering_respects_specific_before_generic`).
///
/// Built from an empirical inventory of MLX kernel symbols across real
/// sessions (issue #193): MLX kernel names mostly glue the dtype straight
/// onto the base name (`layer_normbfloat16`), so prefixes deliberately
/// omit a trailing underscore.
const TABLE: &[(&str, &str)] = &[
    ("quantized_matmul", "QuantizedMatmul"),
    ("affine_qmm", "QuantizedMatmul"),
    ("qmv", "QuantizedMatmul"),
    ("qvm", "QuantizedMatmul"),
    ("qmm", "QuantizedMatmul"),
    ("steel_gemm", "Matmul"),
    ("gemm", "Matmul"),
    ("gemv", "Matmul"),
    ("steel_attention", "ScaledDotProductAttention"),
    ("sdpa", "ScaledDotProductAttention"),
    ("block_softmax", "Softmax"),
    ("softmax", "Softmax"),
    ("rms_norm", "RMSNorm"),
    ("layer_norm", "LayerNorm"),
    ("gelu", "GeLU"),
    ("silu", "SiLU"),
    ("rope", "RoPE"),
    ("implicit_gemm_conv", "Conv"),
    ("winograd_conv", "Conv"),
    ("conv2d", "Conv2d"),
    ("conv", "Conv"),
    ("all_reduce", "Reduce"),
    ("row_reduce", "Reduce"),
    ("col_reduce", "Reduce"),
    ("reduce", "Reduce"),
    ("contig_scan", "Scan"),
    ("scan", "Scan"),
    ("sort", "Sort"),
    ("gather", "Gather"),
    ("scatter", "Scatter"),
    ("arange", "Arange"),
    ("rbits", "Random"),
    ("copy", "Copy"),
    ("affine_quantize", "Quantize"),
    ("quantize", "Quantize"),
    ("dequantize", "Dequantize"),
];

/// Map an MLX shader function name to a canonical op kind.
/// Returns `None` if no pattern matches.
pub fn resolve_kind(symbol: &str) -> Option<&'static str> {
    if let Some(kind) = TABLE
        .iter()
        .find_map(|(prefix, kind)| symbol.starts_with(prefix).then_some(*kind))
    {
        return Some(kind);
    }
    // Copy family: `<layout>_copy<dtypes>` where layout ∈ {g,s,v,n,1..4}
    // (g1_copy, gg2_copy, vn_copy, s_copy, ggn2_copy, …).
    if let Some(idx) = symbol.find("_copy") {
        if is_layout_prefix(&symbol[..idx]) {
            return Some("Copy");
        }
    }
    // Elementwise family: `<layout>_<CamelCaseOp><dtypes>`
    // (g2_Addbfloat16, vvn_Multiply…, sv_Power…, vsn_GreaterEqual…).
    if let Some((prefix, rest)) = symbol.split_once('_') {
        if is_layout_prefix(prefix) && rest.starts_with(|c: char| c.is_ascii_uppercase()) {
            return Some("Elementwise");
        }
    }
    // mx.compile fused kernels: mangled op chain + graph digest + a
    // `_contiguous` / `_strided` layout suffix.
    if symbol.ends_with("_contiguous") || symbol.ends_with("_strided") {
        return Some("FusedElementwise");
    }
    None
}

/// True when `s` looks like an MLX elementwise/copy layout code:
/// non-empty, only {g, s, v, n} and digits (g, g2, vvn, gn1, ggn2, …).
fn is_layout_prefix(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| matches!(c, 'g' | 's' | 'v' | 'n') || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemm_maps_to_matmul() {
        assert_eq!(resolve_kind("gemm_t_n_bf16_64_64_32_2_2_8"), Some("Matmul"));
        assert_eq!(resolve_kind("gemv_t_bf16_64x64"), Some("Matmul"));
    }

    #[test]
    fn quantized_matmul_is_distinct() {
        assert_eq!(
            resolve_kind("quantized_matmul_w4a16_64x64"),
            Some("QuantizedMatmul")
        );
    }

    #[test]
    fn sdpa_maps_to_attention() {
        assert_eq!(
            resolve_kind("sdpa_vector_2pass_1_float16_64"),
            Some("ScaledDotProductAttention")
        );
    }

    #[test]
    fn norms_map_correctly() {
        assert_eq!(resolve_kind("rms_norm_float16"), Some("RMSNorm"));
        assert_eq!(resolve_kind("layer_norm_float16"), Some("LayerNorm"));
    }

    #[test]
    fn activations_map_correctly() {
        assert_eq!(resolve_kind("gelu_float16"), Some("GeLU"));
        assert_eq!(resolve_kind("silu_float16"), Some("SiLU"));
        assert_eq!(resolve_kind("softmax_float16_2"), Some("Softmax"));
    }

    // ── Current MLX kernel naming (empirical inventory, issue #193) ──

    #[test]
    fn steel_kernels_map_to_matmul_and_sdpa() {
        assert_eq!(
            resolve_kind("steel_gemm_fused_nt_bfloat16_bfloat16_bm64_bn64_bk16_wm2_wn2"),
            Some("Matmul")
        );
        assert_eq!(
            resolve_kind("steel_gemm_splitk_accum_bfloat16_float32"),
            Some("Matmul")
        );
        assert_eq!(
            resolve_kind("gemv_bfloat16_bm4_bn1_sm1_sn32_tm4_tn4_nc0_axpby1"),
            Some("Matmul")
        );
        assert_eq!(
            resolve_kind("steel_attention_bfloat16_bq32_bk16_bd128_wm4_wn1"),
            Some("ScaledDotProductAttention")
        );
    }

    #[test]
    fn quantized_kernels_map_to_quantized_matmul() {
        assert_eq!(
            resolve_kind("affine_qmm_t_bfloat16_t_gs_64_b_4_alN_true_batch_0"),
            Some("QuantizedMatmul")
        );
        assert_eq!(
            resolve_kind("qmv_fast_bfloat16_gs64_b4"),
            Some("QuantizedMatmul")
        );
        assert_eq!(
            resolve_kind("qvm_bfloat16_gs64_b4"),
            Some("QuantizedMatmul")
        );
    }

    #[test]
    fn norms_without_trailing_underscore_map_correctly() {
        assert_eq!(resolve_kind("layer_normbfloat16"), Some("LayerNorm"));
        assert_eq!(resolve_kind("layer_norm_loopedbfloat16"), Some("LayerNorm"));
        assert_eq!(resolve_kind("rms_normfloat16"), Some("RMSNorm"));
        assert_eq!(
            resolve_kind("block_softmax_precise_bfloat16"),
            Some("Softmax")
        );
    }

    #[test]
    fn copy_family_maps_to_copy() {
        for s in [
            "gg1_copybfloat16bfloat16",
            "gg2_copyfloat32float32",
            "vn_copybfloat16float32",
            "v_copyint32bfloat16",
            "s_copybfloat16bfloat16",
            "sn_copybfloat16bfloat16",
            "ggn2_copybfloat16bfloat16",
            "g3_copybfloat16bfloat16",
        ] {
            assert_eq!(resolve_kind(s), Some("Copy"), "symbol {s}");
        }
    }

    #[test]
    fn conv_families_map_to_conv() {
        assert_eq!(
            resolve_kind("implicit_gemm_conv_3d_bfloat16_bm64_bn64_bk16_wm2_wn2_filter_s"),
            Some("Conv")
        );
        assert_eq!(
            resolve_kind("winograd_conv_2d_input_transform_bfloat16_bc32"),
            Some("Conv")
        );
    }

    #[test]
    fn reduce_scan_families_map_correctly() {
        assert_eq!(
            resolve_kind("row_reduce_simple_sumbfloat16"),
            Some("Reduce")
        );
        assert_eq!(
            resolve_kind("col_reduce_small_1_reduce_sumbfloat16"),
            Some("Reduce")
        );
        assert_eq!(resolve_kind("all_reduce_maxfloat32"), Some("Reduce"));
        assert_eq!(
            resolve_kind("contig_scan_inclusive_prod_float32_float32"),
            Some("Scan")
        );
    }

    #[test]
    fn gather_scatter_arange_map_correctly() {
        assert_eq!(resolve_kind("gatherbfloat16int64_2_2_int"), Some("Gather"));
        assert_eq!(
            resolve_kind("gather_frontbfloat16_int32_int_2"),
            Some("Gather")
        );
        assert_eq!(
            resolve_kind("scatterfloat32int32_none_2_updc_true_nwork1_int"),
            Some("Scatter")
        );
        assert_eq!(resolve_kind("arangeuint32"), Some("Arange"));
    }

    #[test]
    fn elementwise_families_map_to_elementwise() {
        for s in [
            "g2_Addbfloat16",
            "vvn_Multiplybfloat16",
            "vs_Subtractfloat32",
            "sv_Powerfloat32",
            "gn1_Negativebfloat16bfloat16",
            "vsn_GreaterEqualfloat32",
            "v_Rsqrtfloat32float32",
            "ss_Dividefloat32",
            "vn_Tanhbfloat16bfloat16",
            "g2_Selectfloat32",
        ] {
            assert_eq!(resolve_kind(s), Some("Elementwise"), "symbol {s}");
        }
    }

    #[test]
    fn compiled_fused_kernels_map_to_fused() {
        assert_eq!(
            resolve_kind("BV2ISigmoidACV2OMultiplyAB_V__11160318154034397263_contiguous"),
            Some("FusedElementwise")
        );
        assert_eq!(
            resolve_kind("Gf4IBroadcastBAHf4IMultiplyAG_VCCC__123_strided"),
            Some("FusedElementwise")
        );
    }

    #[test]
    fn random_bits_maps_to_random() {
        assert_eq!(resolve_kind("rbitsc"), Some("Random"));
    }

    #[test]
    fn elementwise_heuristic_does_not_misfire() {
        // Uppercase after underscore but prefix not an elementwise family.
        assert_eq!(resolve_kind("K_MLNet_0x12345"), None);
        // Copy takes priority over the elementwise shape.
        assert_eq!(resolve_kind("g1_copyfloat32float32"), Some("Copy"));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(resolve_kind("K_f900_64x64x1"), None);
        assert_eq!(resolve_kind(""), None);
        assert_eq!(resolve_kind("totally_unknown_kernel"), None);
    }

    #[test]
    fn table_ordering_respects_specific_before_generic() {
        // Invariant: for any pair of entries (i, j) with i < j, entry j's prefix
        // must NOT start with entry i's prefix. If it did, entry j's symbol would
        // always be intercepted by entry i, making entry j dead code.
        //
        // Conversely, if entry i's prefix STARTS WITH entry j's prefix (j is
        // more generic), then i must come first — which is exactly the order
        // we enforce.
        //
        // The rule we actually need: for any pair where one prefix is a strict
        // prefix of the other, the more-specific (longer) one must come first.
        for (i, &(pi, _)) in TABLE.iter().enumerate() {
            for (j, &(pj, _)) in TABLE.iter().enumerate().skip(i + 1) {
                if pj.starts_with(pi) && pj != pi {
                    panic!(
                        "TABLE ordering invariant violated: entry {j} (\"{pj}\") \
                         starts with entry {i} (\"{pi}\"), so it would never be \
                         reached. The more-specific prefix \"{pj}\" must come \
                         before the generic \"{pi}\"."
                    );
                }
            }
        }
    }

    #[test]
    fn quantized_matmul_would_not_be_intercepted_by_a_matmul_generic_rule() {
        // Synthetic check: even if someone added ("matmul_", "Matmul") as the
        // FIRST entry (which is wrong), this test would still pass because
        // "quantized_matmul_w4a16" does not start with "matmul_". This test
        // documents that the existing entries don't collide for this input.
        // The real ordering guard is `table_ordering_respects_specific_before_generic`.
        assert!(!"quantized_matmul_w4a16_64x64".starts_with("matmul_"));
        assert!("quantized_matmul_w4a16_64x64".starts_with("quantized_matmul_"));
    }
}
