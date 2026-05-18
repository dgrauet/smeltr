//! Best-effort mapping from MLX shader function names to canonical op kinds.
//!
//! Returned strings are intended for display in breakdown UIs. The table is
//! MLX-version-sensitive and intended to evolve as MLX adds or renames
//! shaders.

/// Map an MLX shader function name to a canonical op kind.
/// Returns `None` if no pattern matches.
pub fn resolve_kind(symbol: &str) -> Option<&'static str> {
    // Order matters: more-specific prefixes must come first.
    const TABLE: &[(&str, &str)] = &[
        ("quantized_matmul_", "QuantizedMatmul"),
        ("gemm_", "Matmul"),
        ("gemv_", "Matmul"),
        ("sdpa_", "ScaledDotProductAttention"),
        ("softmax_", "Softmax"),
        ("rms_norm_", "RMSNorm"),
        ("layer_norm_", "LayerNorm"),
        ("gelu_", "GeLU"),
        ("silu_", "SiLU"),
        ("rope_", "RoPE"),
        ("conv2d_", "Conv2d"),
        ("conv_", "Conv"),
        ("reduce_", "Reduce"),
        ("scan_", "Scan"),
        ("sort_", "Sort"),
        ("copy_", "Copy"),
        ("quantize_", "Quantize"),
        ("dequantize_", "Dequantize"),
    ];
    TABLE
        .iter()
        .find_map(|(prefix, kind)| symbol.starts_with(prefix).then_some(*kind))
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

    #[test]
    fn unknown_returns_none() {
        assert_eq!(resolve_kind("K_f900_64x64x1"), None);
        assert_eq!(resolve_kind(""), None);
        assert_eq!(resolve_kind("totally_unknown_kernel"), None);
    }
}
