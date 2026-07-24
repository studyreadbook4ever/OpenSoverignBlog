use serde::{Deserialize, Serialize};

use crate::GateError;

pub const KAKAO_ADFIT_SCRIPT_URL: &str = "https://t1.kakaocdn.net/kas/static/ba.min.js";
pub const KAKAO_ADFIT_POLICY_VERSION: &str = "kakao-adfit/1";
pub const KAKAO_ADFIT_CONSENT_PURPOSE_IDS: [&str; 3] =
    ["ads.delivery", "ads.measurement", "ads.personalization"];

const MIN_UNIT_ID_LENGTH: usize = 8;
const MAX_UNIT_ID_LENGTH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KakaoAdFitPlacement {
    Top,
    Bottom,
}

impl KakaoAdFitPlacement {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KakaoAdFitViewport {
    Pc,
    Mobile,
}

impl KakaoAdFitViewport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pc => "pc",
            Self::Mobile => "mobile",
        }
    }

    pub const fn dimensions(self) -> (u16, u16) {
        match self {
            Self::Pc => (728, 90),
            Self::Mobile => (320, 100),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct KakaoAdFitUnitId(String);

impl KakaoAdFitUnitId {
    pub fn parse(value: String) -> Result<Self, GateError> {
        let valid = value.trim() == value
            && value.starts_with("DAN-")
            && (MIN_UNIT_ID_LENGTH..=MAX_UNIT_ID_LENGTH).contains(&value.len())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        if !valid {
            return Err(GateError::InvalidKakaoAdFit(
                "unit ids must be 8-128 characters, start with DAN-, and contain only ASCII letters, digits, - or _",
            ));
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for KakaoAdFitUnitId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted Kakao AdFit unit id]")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KakaoAdFitUnits {
    pc_top: KakaoAdFitUnitId,
    pc_bottom: KakaoAdFitUnitId,
    mobile_top: KakaoAdFitUnitId,
    mobile_bottom: KakaoAdFitUnitId,
}

impl KakaoAdFitUnits {
    pub fn new(
        pc_top: String,
        pc_bottom: String,
        mobile_top: String,
        mobile_bottom: String,
    ) -> Result<Self, GateError> {
        let units = Self {
            pc_top: KakaoAdFitUnitId::parse(pc_top)?,
            pc_bottom: KakaoAdFitUnitId::parse(pc_bottom)?,
            mobile_top: KakaoAdFitUnitId::parse(mobile_top)?,
            mobile_bottom: KakaoAdFitUnitId::parse(mobile_bottom)?,
        };
        let values = [
            units.pc_top.expose(),
            units.pc_bottom.expose(),
            units.mobile_top.expose(),
            units.mobile_bottom.expose(),
        ];
        let distinct = values
            .iter()
            .enumerate()
            .all(|(index, value)| !values[..index].contains(value));
        if !distinct {
            return Err(GateError::InvalidKakaoAdFit(
                "top, bottom, PC, and mobile placements require four distinct unit ids",
            ));
        }
        Ok(units)
    }

    pub fn unit(
        &self,
        placement: KakaoAdFitPlacement,
        viewport: KakaoAdFitViewport,
    ) -> &KakaoAdFitUnitId {
        match (placement, viewport) {
            (KakaoAdFitPlacement::Top, KakaoAdFitViewport::Pc) => &self.pc_top,
            (KakaoAdFitPlacement::Bottom, KakaoAdFitViewport::Pc) => &self.pc_bottom,
            (KakaoAdFitPlacement::Top, KakaoAdFitViewport::Mobile) => &self.mobile_top,
            (KakaoAdFitPlacement::Bottom, KakaoAdFitViewport::Mobile) => &self.mobile_bottom,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units() -> KakaoAdFitUnits {
        KakaoAdFitUnits::new(
            "DAN-PcTop1234".into(),
            "DAN-PcBottom1234".into(),
            "DAN-MobileTop1234".into(),
            "DAN-MobileBottom1234".into(),
        )
        .unwrap()
    }

    #[test]
    fn placement_mapping_has_fixed_official_banner_dimensions() {
        assert_eq!(KakaoAdFitViewport::Pc.dimensions(), (728, 90));
        assert_eq!(KakaoAdFitViewport::Mobile.dimensions(), (320, 100));
        assert_eq!(
            units()
                .unit(KakaoAdFitPlacement::Bottom, KakaoAdFitViewport::Mobile)
                .expose(),
            "DAN-MobileBottom1234"
        );
    }

    #[test]
    fn unit_ids_fail_closed_on_bad_shape_or_reuse() {
        assert!(KakaoAdFitUnitId::parse("not-a-unit".into()).is_err());
        assert!(KakaoAdFitUnitId::parse(" DAN-Spaced1234".into()).is_err());
        assert!(
            KakaoAdFitUnits::new(
                "DAN-Reused1234".into(),
                "DAN-Reused1234".into(),
                "DAN-MobileTop1234".into(),
                "DAN-MobileBottom1234".into(),
            )
            .is_err()
        );
    }

    #[test]
    fn debug_output_never_exposes_operator_unit_ids() {
        let debug = format!("{:?}", units());
        assert!(!debug.contains("DAN-"));
        assert!(debug.contains("redacted Kakao AdFit unit id"));
    }
}
