//! PAW3395SE logical DPI conversion, transcribed from the retained frontend.
//!
//! Source ranges are UTF-16 code-unit offsets in the validated JS artifact:
//! - A2t mapping: `[11173816, 11178945)`
//! - `h2t`: `[11179069, 11179322)`
//! - `v2t`: `[11179379, 11179738)`

use crate::dpi_map::PAW3395SE_DPI_MAP;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodedDpi {
    pub x_dpi: u8,
    pub y_dpi: u8,
    pub dpi_ex: u8,
}

/// Mirrors `h2t` for the PAW3395SE table. An unknown encoded value falls back
/// to 18,000 exactly as the source does; the original encoded bytes remain in
/// `DpiStage::raw` for lossless inspection.
pub fn decode_logical(x_dpi: u8, y_dpi: u8, dpi_ex: u8) -> (u16, u16) {
    let x_double = dpi_ex & 0x02 != 0;
    let y_double = dpi_ex & 0x20 != 0;
    let x = PAW3395SE_DPI_MAP
        .iter()
        .find(|entry| entry.hex == x_dpi)
        .map(|entry| if x_double { entry.double } else { entry.target })
        .filter(|value| *value != 0)
        .unwrap_or(18_000);
    let y = PAW3395SE_DPI_MAP
        .iter()
        .find(|entry| entry.hex == y_dpi)
        .map(|entry| if y_double { entry.double } else { entry.target })
        .filter(|value| *value != 0)
        .unwrap_or(18_000);
    (x, y)
}

/// Mirrors `v2t` including its 50-DPI rounding and its adjustment above
/// 10,000 DPI. Errors replace the source's failed `.find(...).hex` access for
/// values that have no exact table entry.
pub fn encode_logical(x_dpi: u16, y_dpi: u16) -> Result<EncodedDpi, DpiError> {
    let x = source_round_to_50(x_dpi);
    let y = source_round_to_50(y_dpi);
    let x = source_adjust_above_10k(x);
    let y = source_adjust_above_10k(y);
    let (x_code, x_double) = lookup_target(x).ok_or(DpiError::NoSourceEncoding {
        axis: DpiAxis::X,
        requested: x_dpi,
        rounded: x,
    })?;
    let (y_code, y_double) = lookup_target(y).ok_or(DpiError::NoSourceEncoding {
        axis: DpiAxis::Y,
        requested: y_dpi,
        rounded: y,
    })?;

    Ok(EncodedDpi {
        x_dpi: x_code,
        y_dpi: y_code,
        dpi_ex: (if x_double { 0x02 } else { 0 }) | (if y_double { 0x20 } else { 0 }),
    })
}

fn source_round_to_50(value: u16) -> u16 {
    // For nonnegative source inputs Math.round(x / 50) is floor(x / 50 + 0.5).
    (((u32::from(value) + 25) / 50) * 50).min(u32::from(u16::MAX)) as u16
}

fn source_adjust_above_10k(value: u16) -> u16 {
    if value > 10_000 && value % 100 != 0 {
        value.saturating_add(50)
    } else {
        value
    }
}

fn lookup_target(value: u16) -> Option<(u8, bool)> {
    PAW3395SE_DPI_MAP.iter().find_map(|entry| {
        if entry.target == value {
            Some((entry.hex, false))
        } else if entry.double == value && entry.double != 0 {
            Some((entry.hex, true))
        } else {
            None
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DpiAxis {
    X,
    Y,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DpiError {
    NoSourceEncoding {
        axis: DpiAxis,
        requested: u16,
        rounded: u16,
    },
}

impl core::fmt::Display for DpiError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSourceEncoding {
                axis,
                requested,
                rounded,
            } => write!(
                formatter,
                "no PAW3395SE source encoding for {axis:?} DPI {requested} (rounded to {rounded})"
            ),
        }
    }
}

impl std::error::Error for DpiError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paw3395se_source_vectors_cover_table_and_double_flags() {
        // Source A2t rows: {hex:1,target:50}, {hex:118,target:5050,double:true},
        // and {hex:211,target:9000,double:true}.
        assert_eq!(decode_logical(1, 1, 0), (50, 50));
        assert_eq!(decode_logical(118, 211, 0x22), (10_100, 18_000));
        assert_eq!(decode_logical(255, 0, 0), (18_000, 18_000));
    }

    #[test]
    fn paw3395se_source_inverse_rounds_and_sets_axis_bits() {
        // v2t(10050, 18000): x rounds to 10050, then to 10100; y maps to
        // the doubled 9000 row. Source bit construction sets 0x02 for X and
        // 0x20 for Y.
        assert_eq!(
            encode_logical(10_050, 18_000),
            Ok(EncodedDpi {
                x_dpi: 118,
                y_dpi: 211,
                dpi_ex: 0x22,
            })
        );
        assert_eq!(
            encode_logical(50, 50),
            Ok(EncodedDpi {
                x_dpi: 1,
                y_dpi: 1,
                dpi_ex: 0,
            })
        );
        assert!(matches!(
            encode_logical(18_050, 50),
            Err(DpiError::NoSourceEncoding { axis: DpiAxis::X, .. })
        ));
    }
}
