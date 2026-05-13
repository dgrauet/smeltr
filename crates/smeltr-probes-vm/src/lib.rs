pub mod raw;

pub use raw::{compute_rate, read_sys, VmRaw};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_zero_when_no_delta() {
        let a = VmRaw {
            wired_bytes: 1,
            active_bytes: 2,
            compressed_bytes: 3,
            swap_used_bytes: 4,
            page_outs: 100,
        };
        assert_eq!(compute_rate(&a, &a, 1.0), 0.0);
    }

    #[test]
    fn rate_linear_with_delta() {
        let a = VmRaw {
            wired_bytes: 0,
            active_bytes: 0,
            compressed_bytes: 0,
            swap_used_bytes: 0,
            page_outs: 0,
        };
        let b = VmRaw {
            wired_bytes: 0,
            active_bytes: 0,
            compressed_bytes: 0,
            swap_used_bytes: 0,
            page_outs: 500,
        };
        assert!((compute_rate(&a, &b, 0.5) - 1000.0).abs() < 0.001);
    }

    #[test]
    fn rate_handles_zero_dt_gracefully() {
        let a = VmRaw {
            wired_bytes: 0,
            active_bytes: 0,
            compressed_bytes: 0,
            swap_used_bytes: 0,
            page_outs: 0,
        };
        let b = VmRaw {
            wired_bytes: 0,
            active_bytes: 0,
            compressed_bytes: 0,
            swap_used_bytes: 0,
            page_outs: 10,
        };
        assert_eq!(compute_rate(&a, &b, 0.0), 0.0);
    }
}
