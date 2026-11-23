// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Leader election with a `coordination.k8s.io/v1` Lease.
//!
//! Only the replica that holds the Lease runs the controllers. A standby
//! takes the Lease once the holder stops renewing it for a full lease
//! duration. Every write uses the Lease's resource version, so two replicas
//! can't both win the same transition.

use std::time::Duration;

use k8s_openapi::{
    api::coordination::v1::{Lease, LeaseSpec},
    apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta},
    jiff::{SignedDuration, Timestamp},
};
use kube::{
    Api,
    api::{PostParams, ResourceExt},
};

/// How a replica takes part in the election.
#[derive(Clone, Debug)]
pub struct Election {
    pub name: String,
    pub identity: String,
    /// How long the Lease stays valid without a renewal.
    pub lease_duration: Duration,
    /// How often the holder renews the Lease, and how often a standby retries.
    pub retry_period: Duration,
}

/// The holder could not renew the Lease before it expired.
#[derive(Debug, thiserror::Error)]
#[error("lost the leader Lease")]
pub struct LeaseLost;

/// Whether `identity` may take or renew the Lease at `now`.
pub fn claimable(spec: Option<&LeaseSpec>, identity: &str, now: Timestamp) -> bool {
    let Some(spec) = spec else { return true };
    match spec.holder_identity.as_deref() {
        None | Some("") => true,
        Some(holder) if holder == identity => true,
        Some(_) => {
            let Some(renewed) = spec.renew_time.as_ref().or(spec.acquire_time.as_ref()) else {
                return true;
            };
            let duration = SignedDuration::from_secs(i64::from(
                spec.lease_duration_seconds.unwrap_or_default().max(1),
            ));
            renewed
                .0
                .checked_add(duration)
                .is_ok_and(|expiry| expiry < now)
        }
    }
}

/// The Lease spec after `identity` takes or renews it at `now`.
pub fn claim(spec: Option<&LeaseSpec>, election: &Election, now: Timestamp) -> LeaseSpec {
    let previous = spec.cloned().unwrap_or_default();
    let renewing = previous.holder_identity.as_deref() == Some(election.identity.as_str());
    LeaseSpec {
        holder_identity: Some(election.identity.clone()),
        lease_duration_seconds: Some(
            i32::try_from(election.lease_duration.as_secs()).unwrap_or(i32::MAX),
        ),
        acquire_time: if renewing {
            previous.acquire_time.clone()
        } else {
            Some(MicroTime(now))
        },
        renew_time: Some(MicroTime(now)),
        lease_transitions: Some(
            previous.lease_transitions.unwrap_or_default() + i32::from(!renewing),
        ),
        ..previous
    }
}

impl Election {
    /// Take or renew the Lease. `Ok(false)` means another replica holds it,
    /// or another replica changed it first.
    pub async fn try_claim(&self, api: &Api<Lease>) -> Result<bool, kube::Error> {
        let now = Timestamp::now();
        let Some(current) = api.get_opt(&self.name).await? else {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(self.name.clone()),
                    ..ObjectMeta::default()
                },
                spec: Some(claim(None, self, now)),
            };
            return match api.create(&PostParams::default(), &lease).await {
                Ok(_) => Ok(true),
                Err(kube::Error::Api(status)) if status.code == 409 => Ok(false),
                Err(error) => Err(error),
            };
        };
        if !claimable(current.spec.as_ref(), &self.identity, now) {
            return Ok(false);
        }
        let mut next = current.clone();
        next.spec = Some(claim(current.spec.as_ref(), self, now));
        // The resource version from the read makes this a compare-and-swap.
        match api
            .replace(&current.name_any(), &PostParams::default(), &next)
            .await
        {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(status)) if status.code == 409 => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Wait until this replica holds the Lease.
    pub async fn acquire(&self, api: &Api<Lease>) {
        loop {
            match self.try_claim(api).await {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => eprintln!("leader election: {error}"),
            }
            tokio::time::sleep(self.retry_period).await;
        }
    }

    /// Renew the Lease until a renewal can't succeed before it expires.
    pub async fn hold(&self, api: &Api<Lease>) -> LeaseLost {
        let mut last_renewal = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(self.retry_period).await;
            match self.try_claim(api).await {
                Ok(true) => last_renewal = tokio::time::Instant::now(),
                // Another replica holds the Lease: stop at once.
                Ok(false) => return LeaseLost,
                Err(error) => eprintln!("leader election: renewal failed: {error}"),
            }
            // Stop before the Lease can expire, so two leaders never overlap.
            if last_renewal.elapsed() + self.retry_period >= self.lease_duration {
                return LeaseLost;
            }
        }
    }

    /// Give up the Lease so a standby can take it without waiting.
    pub async fn release(&self, api: &Api<Lease>) {
        let Ok(Some(current)) = api.get_opt(&self.name).await else {
            return;
        };
        let held = current
            .spec
            .as_ref()
            .and_then(|spec| spec.holder_identity.as_deref())
            == Some(self.identity.as_str());
        if !held {
            return;
        }
        let mut next = current.clone();
        if let Some(spec) = next.spec.as_mut() {
            spec.holder_identity = None;
            spec.renew_time = None;
            spec.acquire_time = None;
        }
        let _ = api
            .replace(&current.name_any(), &PostParams::default(), &next)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn election(identity: &str) -> Election {
        Election {
            name: "lease".into(),
            identity: identity.into(),
            lease_duration: Duration::from_secs(15),
            retry_period: Duration::from_secs(5),
        }
    }

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + seconds).unwrap()
    }

    #[test]
    fn an_empty_lease_is_claimable() {
        assert!(claimable(None, "a", at(0)));
        assert!(claimable(Some(&LeaseSpec::default()), "a", at(0)));
    }

    #[test]
    fn another_holder_blocks_until_the_lease_expires() {
        let held = claim(None, &election("a"), at(0));
        assert!(claimable(Some(&held), "a", at(1)));
        assert!(!claimable(Some(&held), "b", at(1)));
        assert!(!claimable(Some(&held), "b", at(15)));
        assert!(claimable(Some(&held), "b", at(16)));
    }

    #[test]
    fn renewing_keeps_the_acquire_time_and_transitions() {
        let first = claim(None, &election("a"), at(0));
        assert_eq!(first.lease_transitions, Some(1));
        let renewed = claim(Some(&first), &election("a"), at(5));
        assert_eq!(renewed.acquire_time, first.acquire_time);
        assert_eq!(renewed.renew_time, Some(MicroTime(at(5))));
        assert_eq!(renewed.lease_transitions, Some(1));
        let taken = claim(Some(&renewed), &election("b"), at(30));
        assert_eq!(taken.holder_identity.as_deref(), Some("b"));
        assert_eq!(taken.acquire_time, Some(MicroTime(at(30))));
        assert_eq!(taken.lease_transitions, Some(2));
    }
}
