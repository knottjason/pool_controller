//! ADS1115 reader abstraction (mockable) + Linux production wrapper.

use std::fmt;

/// Error from an ADS read attempt (open, I2C, or conversion).
#[derive(Debug, Clone)]
pub struct AdsError {
    pub kind: AdsErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdsErrorKind {
    Open,
    I2c,
    Config,
    OutOfRange,
}

impl fmt::Display for AdsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for AdsError {}

/// Trait for single-ended ADS reads (production + mock).
pub trait AdsReader {
    fn read_single_ended(&mut self, channel: u8) -> Result<i16, AdsError>;
}

/// Mock ADC returning a fixed sequence of raw values (cycles).
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct MockAdsReader {
    pub values: Vec<Result<i16, AdsError>>,
    pub index: usize,
}

#[cfg(test)]
impl MockAdsReader {
    #[must_use]
    pub fn always(raw: i16) -> Self {
        Self {
            values: vec![Ok(raw)],
            index: 0,
        }
    }

    #[must_use]
    pub fn failing(kind: AdsErrorKind, message: impl Into<String>) -> Self {
        Self {
            values: vec![Err(AdsError {
                kind,
                message: message.into(),
            })],
            index: 0,
        }
    }
}

#[cfg(test)]
impl AdsReader for MockAdsReader {
    fn read_single_ended(&mut self, _channel: u8) -> Result<i16, AdsError> {
        if self.values.is_empty() {
            return Err(AdsError {
                kind: AdsErrorKind::I2c,
                message: "empty mock".into(),
            });
        }
        let i = self.index % self.values.len();
        self.index = self.index.wrapping_add(1);
        self.values[i].clone()
    }
}

/// Host / missing-hardware stub: every read fails as an open error.
#[cfg(not(target_os = "linux"))]
pub struct UnavailableAds {
    pub device: String,
    pub address: u8,
}

#[cfg(not(target_os = "linux"))]
impl AdsReader for UnavailableAds {
    fn read_single_ended(&mut self, _channel: u8) -> Result<i16, AdsError> {
        Err(AdsError {
            kind: AdsErrorKind::Open,
            message: format!(
                "ADS1115 unavailable ({} @ 0x{:02x})",
                self.device, self.address
            ),
        })
    }
}

/// Take `sample_count` readings with `sample_delay` between them and average.
pub fn sample_average<R: AdsReader + ?Sized>(
    reader: &mut R,
    channel: u8,
    sample_count: u8,
    sample_delay: std::time::Duration,
) -> Result<i16, AdsError> {
    let n = usize::from(sample_count.max(1));
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        samples.push(reader.read_single_ended(channel)?);
        if i + 1 < n && !sample_delay.is_zero() {
            std::thread::sleep(sample_delay);
        }
    }
    crate::temp::convert::average_raw(&samples).ok_or_else(|| AdsError {
        kind: AdsErrorKind::OutOfRange,
        message: "empty sample set".into(),
    })
}

/// Open the platform ADS1115 reader (Linux) or an unavailable stub (host).
pub fn open_ads_reader(device: &str, address: u8) -> Result<Box<dyn AdsReader + Send>, AdsError> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux_ads::LinuxAds1115::open(device, address)?))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = address;
        // Still validate address mapping parity with Linux.
        match address {
            0x48..=0x4B => {}
            _ => {
                return Err(AdsError {
                    kind: AdsErrorKind::Config,
                    message: format!("unsupported ADS1115 address 0x{address:02x}"),
                });
            }
        }
        Ok(Box::new(UnavailableAds {
            device: device.to_string(),
            address,
        }))
    }
}

#[cfg(target_os = "linux")]
mod linux_ads {
    use ads1x1x::{Ads1x1x, FullScaleRange, TargetAddr, channel};
    use linux_embedded_hal::I2cdev;
    use nb::block;

    use super::{AdsError, AdsErrorKind, AdsReader};

    type Ads1115 =
        Ads1x1x<I2cdev, ads1x1x::ic::Ads1115, ads1x1x::ic::Resolution16Bit, ads1x1x::mode::OneShot>;

    pub struct LinuxAds1115 {
        adc: Ads1115,
    }

    impl LinuxAds1115 {
        pub fn open(device: &str, address: u8) -> Result<Self, AdsError> {
            let i2c = I2cdev::new(device).map_err(|e| AdsError {
                kind: AdsErrorKind::Open,
                message: format!("open {device}: {e}"),
            })?;
            let addr = target_addr(address)?;
            let mut adc = Ads1x1x::new_ads1115(i2c, addr);
            adc.set_full_scale_range(FullScaleRange::Within4_096V)
                .map_err(|e| AdsError {
                    kind: AdsErrorKind::Config,
                    message: format!("set PGA ±4.096V: {e:?}"),
                })?;
            Ok(Self { adc })
        }
    }

    impl AdsReader for LinuxAds1115 {
        fn read_single_ended(&mut self, ch: u8) -> Result<i16, AdsError> {
            let result = match ch {
                0 => block!(self.adc.read(channel::SingleA0)),
                1 => block!(self.adc.read(channel::SingleA1)),
                2 => block!(self.adc.read(channel::SingleA2)),
                3 => block!(self.adc.read(channel::SingleA3)),
                _ => {
                    return Err(AdsError {
                        kind: AdsErrorKind::Config,
                        message: format!("channel {ch} out of range (0..=3)"),
                    });
                }
            };
            result.map_err(|e| AdsError {
                kind: AdsErrorKind::I2c,
                message: format!("ADS1115 read: {e:?}"),
            })
        }
    }

    fn target_addr(address: u8) -> Result<TargetAddr, AdsError> {
        match address {
            0x48 => Ok(TargetAddr::Gnd),
            0x49 => Ok(TargetAddr::Vdd),
            0x4A => Ok(TargetAddr::Sda),
            0x4B => Ok(TargetAddr::Scl),
            _ => Err(AdsError {
                kind: AdsErrorKind::Config,
                message: format!("unsupported ADS1115 address 0x{address:02x}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn mock_average_and_fault() {
        let mut ok = MockAdsReader {
            values: vec![Ok(1000), Ok(1100), Ok(900)],
            index: 0,
        };
        let avg = sample_average(&mut ok, 0, 3, Duration::ZERO).unwrap();
        assert_eq!(avg, 1000);

        let mut bad = MockAdsReader::failing(AdsErrorKind::I2c, "nack");
        let err = sample_average(&mut bad, 0, 1, Duration::ZERO).unwrap_err();
        assert_eq!(err.kind, AdsErrorKind::I2c);

        let mut fixed = MockAdsReader::always(2000);
        assert_eq!(fixed.read_single_ended(0).unwrap(), 2000);

        let Err(err) = open_ads_reader("/dev/null", 0x40) else {
            panic!("expected Config error for bad address");
        };
        assert_eq!(err.kind, AdsErrorKind::Config);
    }
}
