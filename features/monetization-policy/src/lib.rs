use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use url::Url;

mod first_party_ad;
mod kakao_adfit;

pub use first_party_ad::{
    AuthorizedFirstPartyAd, DisclosureLabel, FirstPartyAd, FirstPartyAdKind, FirstPartyImage,
    NamedAdSlot, StaticAdRenderPlan, StaticDeliveryPolicy,
};
pub use kakao_adfit::{
    KAKAO_ADFIT_CONSENT_PURPOSE_IDS, KAKAO_ADFIT_POLICY_VERSION, KAKAO_ADFIT_SCRIPT_URL,
    KakaoAdFitPlacement, KakaoAdFitUnitId, KakaoAdFitUnits, KakaoAdFitViewport,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentDecision {
    Granted,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAccessBasis {
    Consent,
    TransmissionException,
    RequestedServiceException,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDeclaration {
    pub id: String,
    pub url: Url,
    pub kind: String,
    pub purpose_ids: BTreeSet<String>,
    pub terminal_access_basis: TerminalAccessBasis,
    #[serde(default)]
    pub operator_confirmed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentSnapshot {
    pub policy_version: String,
    #[serde(default)]
    pub decisions: BTreeMap<String, ConsentDecision>,
}

pub struct ResourceGate {
    declared: BTreeMap<String, ResourceDeclaration>,
}

impl ResourceGate {
    pub fn new(resources: Vec<ResourceDeclaration>) -> Result<Self, GateError> {
        let mut declared = BTreeMap::new();
        for resource in resources {
            if resource.id.trim().is_empty()
                || resource.kind.trim().is_empty()
                || resource.url.scheme() != "https"
                || declared.insert(resource.id.clone(), resource).is_some()
            {
                return Err(GateError::InvalidDeclaration);
            }
        }
        Ok(Self { declared })
    }

    pub fn authorize(
        &self,
        resource_id: &str,
        consent: Option<&ConsentSnapshot>,
    ) -> Result<&ResourceDeclaration, GateError> {
        let resource = self
            .declared
            .get(resource_id)
            .ok_or(GateError::UndeclaredResource)?;
        match resource.terminal_access_basis {
            TerminalAccessBasis::Consent => {
                let consent = consent.ok_or(GateError::ConsentRequired)?;
                let all_granted = !resource.purpose_ids.is_empty()
                    && resource.purpose_ids.iter().all(|purpose| {
                        consent.decisions.get(purpose) == Some(&ConsentDecision::Granted)
                    });
                if !all_granted {
                    return Err(GateError::ConsentRequired);
                }
            }
            TerminalAccessBasis::TransmissionException
            | TerminalAccessBasis::RequestedServiceException => {
                if !resource.operator_confirmed
                    || resource
                        .rationale
                        .as_ref()
                        .is_none_or(|value| value.trim().is_empty())
                {
                    return Err(GateError::UnconfirmedException);
                }
            }
        }
        Ok(resource)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdDisclosure {
    pub ad_id: String,
    pub slot_id: String,
    pub visible_label: String,
    pub on_behalf_of: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paid_by: Option<String>,
    pub targeting_mode: String,
    #[serde(default)]
    pub main_targeting_parameters: Vec<String>,
    #[serde(default)]
    pub data_processing_purpose_ids: Vec<String>,
}

impl AdDisclosure {
    pub fn validate(&self) -> Result<(), GateError> {
        if self.ad_id.trim().is_empty()
            || self.slot_id.trim().is_empty()
            || self.visible_label.trim().is_empty()
            || self.on_behalf_of.trim().is_empty()
        {
            return Err(GateError::MissingDisclosure);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    #[error("resource declaration is invalid")]
    InvalidDeclaration,
    #[error("resource was not declared by the active policy")]
    UndeclaredResource,
    #[error("resource remains blocked until every purpose is granted")]
    ConsentRequired,
    #[error("strictly necessary exception lacks operator confirmation and rationale")]
    UnconfirmedException,
    #[error("advertising disclosure is incomplete")]
    MissingDisclosure,
    #[error("static first-party ad is invalid: {0}")]
    InvalidStaticAd(&'static str),
    #[error("static first-party ad requested a restricted capability: {0}")]
    RestrictedStaticDelivery(&'static str),
    #[error("Kakao AdFit configuration is invalid: {0}")]
    InvalidKakaoAdFit(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource() -> ResourceDeclaration {
        ResourceDeclaration {
            id: "provider-script".into(),
            url: Url::parse("https://ads.example/script.js").unwrap(),
            kind: "script".into(),
            purpose_ids: BTreeSet::from(["contextual_ads".into(), "measurement".into()]),
            terminal_access_basis: TerminalAccessBasis::Consent,
            operator_confirmed: false,
            rationale: None,
        }
    }

    #[test]
    fn fresh_or_partial_consent_never_loads_an_ad_resource() {
        let gate = ResourceGate::new(vec![resource()]).unwrap();
        assert_eq!(
            gate.authorize("provider-script", None),
            Err(GateError::ConsentRequired)
        );
        let partial = ConsentSnapshot {
            policy_version: "1".into(),
            decisions: BTreeMap::from([("contextual_ads".into(), ConsentDecision::Granted)]),
        };
        assert_eq!(
            gate.authorize("provider-script", Some(&partial)),
            Err(GateError::ConsentRequired)
        );
    }

    #[test]
    fn every_declared_purpose_must_be_granted() {
        let gate = ResourceGate::new(vec![resource()]).unwrap();
        let consent = ConsentSnapshot {
            policy_version: "1".into(),
            decisions: BTreeMap::from([
                ("contextual_ads".into(), ConsentDecision::Granted),
                ("measurement".into(), ConsentDecision::Granted),
            ]),
        };
        assert!(gate.authorize("provider-script", Some(&consent)).is_ok());
    }

    #[test]
    fn an_exception_is_not_created_by_naming_it_essential() {
        let mut declared = resource();
        declared.terminal_access_basis = TerminalAccessBasis::RequestedServiceException;
        declared.purpose_ids.clear();
        let gate = ResourceGate::new(vec![declared]).unwrap();
        assert_eq!(
            gate.authorize("provider-script", None),
            Err(GateError::UnconfirmedException)
        );
    }
}
