//! Pure ADC → resistance → Steinhart-B → °F conversion (no I2C).

/// ADS1115 positive full-scale counts for single-ended readings (±FS PGA).
pub const ADS1115_FS_COUNTS: f64 = 32_767.0;

/// Convert a raw ADS1115 single-ended reading to thermistor resistance (ohms).
///
/// Mirrors ESP: `R = SERIES / (FS/raw - 1)`. Rejects raw outside `[raw_min, raw_max]`
/// and non-positive resistance.
#[must_use]
pub fn ads_raw_to_resistance(
    raw: i16,
    series_ohms: f64,
    full_scale_counts: f64,
    raw_min: i16,
    raw_max: i16,
) -> Option<f64> {
    if raw < raw_min || raw > raw_max {
        return None;
    }
    let raw_f = f64::from(raw);
    if raw_f <= 0.0 || full_scale_counts <= 0.0 || series_ohms <= 0.0 {
        return None;
    }
    let ratio = full_scale_counts / raw_f - 1.0;
    if ratio <= 0.0 {
        return None;
    }
    let resistance = series_ohms / ratio;
    if resistance.is_finite() && resistance > 0.0 {
        Some(resistance)
    } else {
        None
    }
}

/// Steinhart-B (simplified) resistance → °C. Identical to ESP `getTemp`.
#[must_use]
pub fn resistance_to_celsius(
    resistance: f64,
    nominal_ohms: f64,
    b: f64,
    t0_c: f64,
    celsius_min: f64,
    celsius_max: f64,
) -> Option<f64> {
    if resistance <= 0.0 || nominal_ohms <= 0.0 || b <= 0.0 {
        return None;
    }
    let mut kelvin = resistance / nominal_ohms; // R/Ro
    if kelvin <= 0.0 {
        return None;
    }
    kelvin = kelvin.ln(); // ln(R/Ro)
    kelvin *= 1.0 / b; // 1/B * ln(R/Ro)
    kelvin += 1.0 / (t0_c + 273.15);
    if kelvin == 0.0 {
        return None;
    }
    kelvin = 1.0 / kelvin;
    let celsius = kelvin - 273.15;
    if !celsius.is_finite() || celsius < celsius_min || celsius > celsius_max {
        return None;
    }
    Some(celsius)
}

/// °C → °F.
#[must_use]
pub const fn celsius_to_fahrenheit(celsius: f64) -> f64 {
    (celsius * 1.8) + 32.0
}

/// Full pipeline: raw ADC average → °F.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn raw_to_fahrenheit(
    raw: i16,
    series_ohms: f64,
    nominal_ohms: f64,
    b: f64,
    t0_c: f64,
    raw_min: i16,
    raw_max: i16,
    celsius_min: f64,
    celsius_max: f64,
) -> Option<f64> {
    let r = ads_raw_to_resistance(raw, series_ohms, ADS1115_FS_COUNTS, raw_min, raw_max)?;
    let c = resistance_to_celsius(r, nominal_ohms, b, t0_c, celsius_min, celsius_max)?;
    Some(celsius_to_fahrenheit(c))
}

/// Average integer samples (truncating toward zero after mean).
#[must_use]
pub fn average_raw(samples: &[i16]) -> Option<i16> {
    if samples.is_empty() {
        return None;
    }
    let sum: i64 = samples.iter().map(|s| i64::from(*s)).sum();
    #[allow(clippy::cast_possible_truncation)]
    let avg = (sum / i64::try_from(samples.len()).ok()?) as i16;
    Some(avg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERIES: f64 = 10_000.0;
    const NOMINAL: f64 = 10_500.0;
    const B: f64 = 3950.0;
    const T0: f64 = 25.0;
    const RAW_MIN: i16 = 80;
    const RAW_MAX: i16 = 32_600;

    #[test]
    fn resistance_at_nominal_gives_about_25c() {
        // R = Ro → ~25 °C
        let c = resistance_to_celsius(NOMINAL, NOMINAL, B, T0, -20.0, 60.0).unwrap();
        assert!((c - 25.0).abs() < 0.5, "got {c}");
    }

    #[test]
    fn celsius_to_fahrenheit_25c() {
        assert!((celsius_to_fahrenheit(25.0) - 77.0).abs() < f64::EPSILON);
    }

    #[test]
    fn reject_raw_out_of_range() {
        assert!(ads_raw_to_resistance(10, SERIES, ADS1115_FS_COUNTS, RAW_MIN, RAW_MAX).is_none());
        assert!(
            ads_raw_to_resistance(32_700, SERIES, ADS1115_FS_COUNTS, RAW_MIN, RAW_MAX).is_none()
        );
    }

    #[test]
    fn reject_celsius_out_of_band() {
        // Very high resistance → very cold
        assert!(resistance_to_celsius(1_000_000.0, NOMINAL, B, T0, -20.0, 60.0).is_none());
        // Very low resistance → very hot
        assert!(resistance_to_celsius(100.0, NOMINAL, B, T0, -20.0, 60.0).is_none());
    }

    #[test]
    fn midrange_raw_converts() {
        // Pick raw such that R ≈ Ro: R = SERIES / (FS/raw - 1) = NOMINAL
        // SERIES / NOMINAL = FS/raw - 1 → FS/raw = SERIES/NOMINAL + 1
        // raw = FS / (SERIES/NOMINAL + 1)
        let raw_f = ADS1115_FS_COUNTS / (SERIES / NOMINAL + 1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let raw = raw_f.round() as i16;
        let f =
            raw_to_fahrenheit(raw, SERIES, NOMINAL, B, T0, RAW_MIN, RAW_MAX, -20.0, 60.0).unwrap();
        assert!(
            (f - 77.0).abs() < 1.0,
            "expected ~77°F at nominal, got {f} (raw={raw})"
        );
    }

    #[test]
    fn average_raw_samples() {
        assert_eq!(average_raw(&[100, 200, 300]), Some(200));
        assert!(average_raw(&[]).is_none());
    }
}
