//! Physical lengths, used for image sizes and table column widths.

// ── Units ────────────────────────────────────────────────────────────────────

/// English Metric Units (EMU) per inch. DrawingML picked the EMU so that
/// inches, points and centimetres are all whole numbers of it
/// (ECMA-376 Part 1, §20.1.2.1).
const EMU_PER_INCH: u64 = 914_400;
/// EMU per point, 1/72 inch (ECMA-376 Part 1, §20.1.2.1).
const EMU_PER_PT: u64 = 12_700;
/// EMU per centimetre (ECMA-376 Part 1, §20.1.2.1).
const EMU_PER_CM: u64 = 360_000;
/// EMU per millimetre, a tenth of [`EMU_PER_CM`].
const EMU_PER_MM: u64 = 36_000;
/// EMU per pixel at 96 DPI: `EMU_PER_INCH / 96`.
const EMU_PER_PX: u64 = 9_525;
/// EMU per twip, the twentieth of a point (`dxa`) that WordprocessingML
/// measures table widths in: `EMU_PER_PT / 20`.
const EMU_PER_TWIP: u64 = 635;

// ── Length ───────────────────────────────────────────────────────────────────

/// A physical length, such as the display size of an [`Image`](crate::Image)
/// or the width of a [`Table`](crate::Table) column.
///
/// A length is a whole number of English Metric Units (EMU), the unit
/// DrawingML measures sizes in: 914,400 per inch, 360,000 per centimetre
/// and 12,700 per point (ECMA-376 Part 1, §20.1.2.1), so lengths in any of
/// these units convert exactly. The floating-point constructors round to
/// the nearest EMU.
///
/// The constructors never fail or panic: a negative or NaN length becomes
/// zero, and a length beyond `u64::MAX` EMU (over 500 million km),
/// including infinity, saturates to `u64::MAX` EMU.
///
/// ```
/// use ciclostile::Length;
///
/// assert_eq!(Length::from_inches(1.0), Length::from_px(96));
/// assert_eq!(Length::from_cm(2.54), Length::from_inches(1.0));
/// assert_eq!(Length::from_pt(72.0).as_emu(), 914_400);
/// assert_eq!(Length::from_mm(-5.0), Length::from_emu(0));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Length(u64);

impl Length {
    /// Create a length from English Metric Units (EMU).
    #[must_use]
    pub const fn from_emu(emu: u64) -> Self {
        Self(emu)
    }

    /// Create a length from pixels at 96 DPI (9,525 EMU per pixel).
    #[must_use]
    pub const fn from_px(px: u32) -> Self {
        // A lossless widening, written with `as` because `u64::from` isn't
        // `const`. `u32::MAX * 9_525` is below 2^46: no overflow.
        Self(px as u64 * EMU_PER_PX)
    }

    /// Create a length from typographic points (1/72 inch, 12,700 EMU).
    #[must_use]
    pub const fn from_pt(pt: f64) -> Self {
        Self(round_to_emu(pt, EMU_PER_PT))
    }

    /// Create a length from inches (914,400 EMU).
    #[must_use]
    pub const fn from_inches(inches: f64) -> Self {
        Self(round_to_emu(inches, EMU_PER_INCH))
    }

    /// Create a length from centimetres (360,000 EMU).
    #[must_use]
    pub const fn from_cm(cm: f64) -> Self {
        Self(round_to_emu(cm, EMU_PER_CM))
    }

    /// Create a length from millimetres (36,000 EMU).
    #[must_use]
    pub const fn from_mm(mm: f64) -> Self {
        Self(round_to_emu(mm, EMU_PER_MM))
    }

    /// Return the length in English Metric Units (EMU).
    #[must_use]
    pub const fn as_emu(self) -> u64 {
        self.0
    }

    /// Return the length in twips (1/20 point, 635 EMU), rounded to the
    /// nearest one: the unit of `dxa` table widths and of grid columns
    /// (`ST_TwipsMeasure`, ECMA-376 Part 1).
    pub(crate) const fn as_twips(self) -> u64 {
        let twips = self.0 / EMU_PER_TWIP;
        // 635 is odd, so no remainder is exactly half a twip. `twips + 1`
        // can't overflow: `twips` is at most `u64::MAX / 635`.
        if self.0 % EMU_PER_TWIP > EMU_PER_TWIP / 2 {
            twips + 1
        } else {
            twips
        }
    }
}

/// Convert `value` units of `emu_per_unit` EMU each into a whole number of
/// EMU, rounding to the nearest one (halves round up).
///
/// Float-to-integer `as` casts saturate, which gives the documented
/// behaviour: NaN and negative values become 0, values beyond `u64::MAX`
/// (infinity included) become `u64::MAX`. Hand-rolled because `f64::round`
/// is not `const` on the minimum supported Rust version.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "float-to-int casts saturate as documented; the unit factors and \
              every truncated value below 2^53 convert to f64 exactly"
)]
const fn round_to_emu(value: f64, emu_per_unit: u64) -> u64 {
    let emu = value * emu_per_unit as f64;
    let whole = emu as u64;
    // Below 2^53 the subtraction is exact (Sterbenz lemma); from 2^53 up
    // every f64 is a whole number, so the fraction is 0 - or `whole`
    // already saturated and adding 1 saturates too.
    if emu - whole as f64 >= 0.5 {
        whole.saturating_add(1)
    } else {
        whole
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_convert_to_their_exact_emu_values() {
        assert_eq!(Length::from_emu(42).as_emu(), 42);
        assert_eq!(Length::from_px(1).as_emu(), 9_525);
        assert_eq!(Length::from_pt(1.0).as_emu(), 12_700);
        assert_eq!(Length::from_inches(1.0).as_emu(), 914_400);
        assert_eq!(Length::from_cm(1.0).as_emu(), 360_000);
        assert_eq!(Length::from_mm(1.0).as_emu(), 36_000);
    }

    #[test]
    fn px_are_measured_at_96_dpi() {
        assert_eq!(Length::from_px(96), Length::from_inches(1.0));
        assert_eq!(Length::from_px(4), Length::from_pt(3.0));
    }

    #[test]
    fn equivalent_lengths_in_different_units_are_equal() {
        assert_eq!(Length::from_cm(2.54), Length::from_inches(1.0));
        assert_eq!(Length::from_mm(25.4), Length::from_inches(1.0));
        assert_eq!(Length::from_pt(72.0), Length::from_inches(1.0));
        assert_eq!(Length::from_mm(10.0), Length::from_cm(1.0));
    }

    #[test]
    fn fractional_values_round_to_the_nearest_emu() {
        // In f64, 0.7 * 360_000 is 251_999.999_999_999_97 and
        // 0.07 * 36_000 is 2_520.000_000_000_000_5: truncating would be off.
        assert_eq!(Length::from_cm(0.7).as_emu(), 252_000);
        assert_eq!(Length::from_mm(0.07).as_emu(), 2_520);
        assert_eq!(Length::from_pt(1.15).as_emu(), 14_605);
        assert_eq!(Length::from_pt(0.5).as_emu(), 6_350);
        // 1e-6 in = 0.9144 EMU, 4e-7 in = 0.365_76 EMU.
        assert_eq!(Length::from_inches(1e-6).as_emu(), 1);
        assert_eq!(Length::from_inches(4e-7).as_emu(), 0);
        // Exact halves round up: 1/12_700 pt is 1 EMU, half of it 0.5 EMU.
        assert_eq!(Length::from_pt(0.5 / 12_700.0).as_emu(), 1);
        assert_eq!(Length::from_pt(1.5 / 12_700.0).as_emu(), 2);
    }

    #[test]
    fn negative_and_nan_lengths_become_zero() {
        for length in [
            Length::from_pt(-1.0),
            Length::from_inches(-0.0),
            Length::from_cm(f64::NAN),
            Length::from_mm(-f64::NAN),
            Length::from_inches(f64::NEG_INFINITY),
            Length::from_cm(f64::MIN),
            Length::from_mm(-1e-300),
        ] {
            assert_eq!(length, Length::from_emu(0));
        }
    }

    #[test]
    fn huge_and_infinite_lengths_saturate() {
        for length in [
            Length::from_pt(f64::INFINITY),
            Length::from_inches(f64::MAX),
            Length::from_cm(1e30),
            // Just past u64::MAX EMU once multiplied by 36_000.
            Length::from_mm(5.2e14),
        ] {
            assert_eq!(length.as_emu(), u64::MAX);
        }
        // The largest pixel count doesn't overflow.
        assert_eq!(
            Length::from_px(u32::MAX).as_emu(),
            u64::from(u32::MAX) * 9_525
        );
    }

    #[test]
    fn large_whole_values_convert_exactly() {
        // 3.6e17 EMU (10 million km) is above 2^53, where f64 has no
        // fractional part left, and is exactly representable.
        assert_eq!(Length::from_cm(1e12).as_emu(), 360_000_000_000_000_000);
        assert_eq!(Length::from_emu(u64::MAX).as_emu(), u64::MAX);
    }

    #[test]
    fn constructors_work_in_const_context() {
        const MARGIN: Length = Length::from_cm(2.0);
        assert_eq!(MARGIN.as_emu(), 720_000);
    }

    #[test]
    fn as_twips_rounds_to_the_nearest_twip() {
        let twips = |emu| Length::from_emu(emu).as_twips();
        assert_eq!(twips(0), 0);
        assert_eq!(twips(317), 0);
        assert_eq!(twips(318), 1);
        assert_eq!(twips(635), 1);
        assert_eq!(twips(952), 1);
        assert_eq!(twips(953), 2);
        assert_eq!(Length::from_pt(1.0).as_twips(), 20);
        assert_eq!(Length::from_inches(1.0).as_twips(), 1_440);
        // 1 cm = 566.93 twips.
        assert_eq!(Length::from_cm(1.0).as_twips(), 567);
        // u64::MAX = 635 * 29_049_990_667_259_136 + 255: rounds down...
        assert_eq!(twips(u64::MAX), 29_049_990_667_259_136);
        // ...and 256 EMU less leaves a remainder of 634, which rounds up.
        assert_eq!(twips(u64::MAX - 256), 29_049_990_667_259_136);
    }

    #[test]
    fn lengths_order_by_size() {
        assert_eq!(Length::default(), Length::from_emu(0));
        assert!(Length::from_cm(1.0) < Length::from_inches(1.0));
        assert!(Length::from_pt(1.0) > Length::from_px(1));
    }
}
