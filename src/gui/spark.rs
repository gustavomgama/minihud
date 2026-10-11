//! Pure sparkline math: normalise an fps series into `0..=1` heights.
//!
//! No GUI dependency, so it is unit-tested in the default suite. The egui layer
//! maps the heights onto a rect.

/// Normalise each sample to a `0..=1` height against `max`. An empty series
/// stays empty; a non-positive `max` is flat zero (never a divide by zero).
pub fn normalize(series: &[f32], max: f32) -> Vec<f32> {
    if max <= 0.0 {
        return vec![0.0; series.len()];
    }
    series.iter().map(|&v| (v / max).clamp(0.0, 1.0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_series_is_empty() {
        // Catches: inventing points for no data (a phantom line).
        assert!(normalize(&[], 60.0).is_empty());
    }

    #[test]
    fn scales_by_max_and_clamps_to_one() {
        // Catches: an unclamped height (a line drawn above the rect) or a
        // wrong scale (the shape squashed or clipped).
        let n = normalize(&[0.0, 30.0, 60.0, 120.0], 60.0);
        assert_eq!(n, vec![0.0, 0.5, 1.0, 1.0]);
    }

    #[test]
    fn a_nonpositive_max_is_flat_zero() {
        // Catches: a zero/negative max producing NaN/inf heights (a broken or
        // invisible line).
        assert_eq!(normalize(&[10.0, 20.0], 0.0), vec![0.0, 0.0]);
        assert_eq!(normalize(&[10.0, 20.0], -5.0), vec![0.0, 0.0]);
    }
}
