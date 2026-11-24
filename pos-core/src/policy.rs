//! Store policy that managers edit on the till (Settings): what each role
//! may do, a sales goal and automatic sign-out. Persisted as a `PolicySet`
//! event; on a store network the head hands it to the lanes.

use crate::auth::{Permission, Role, role_allows};
use crate::money::Money;
use alloc::collections::BTreeMap;
use serde::{Deserialize, Serialize};

/// Longest allowed idle time before automatic sign-out (minutes).
pub const MAX_IDLE_MINS: u16 = 240;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Permission bit mask ([`Permission::bit`]) per role. Managers are not
    /// listed: they may always do everything.
    pub roles: BTreeMap<Role, u32>,
    /// Sales goal per shift; zero turns it off.
    pub shift_goal: Money,
    /// Sign out after this many idle minutes (no sale open); 0 = never.
    pub idle_signout_mins: u16,
}

impl Default for Policy {
    fn default() -> Self {
        let mut roles = BTreeMap::new();
        for r in Role::ALL {
            if r != Role::Manager {
                roles.insert(r, default_mask(r));
            }
        }
        Policy {
            roles,
            shift_goal: Money::ZERO,
            idle_signout_mins: 0,
        }
    }
}

/// The built-in permissions of a role as a mask.
pub fn default_mask(role: Role) -> u32 {
    Permission::ALL
        .iter()
        .filter(|p| role_allows(role, **p))
        .fold(0, |m, p| m | p.bit())
}

impl Policy {
    pub fn allows(&self, role: Role, p: Permission) -> bool {
        if role == Role::Manager {
            return true;
        }
        if p.manager_only() {
            return false;
        }
        self.mask(role) & p.bit() != 0
    }

    pub fn mask(&self, role: Role) -> u32 {
        self.roles
            .get(&role)
            .copied()
            .unwrap_or_else(|| default_mask(role))
    }

    /// Grant or remove one permission. Managers and manager-only
    /// permissions cannot be changed (returns false).
    pub fn set(&mut self, role: Role, p: Permission, on: bool) -> bool {
        if role == Role::Manager || p.manager_only() {
            return false;
        }
        let m = self.mask(role);
        self.roles
            .insert(role, if on { m | p.bit() } else { m & !p.bit() });
        true
    }

    pub fn count(&self, role: Role) -> usize {
        Permission::ALL
            .iter()
            .filter(|p| self.allows(role, **p))
            .count()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.shift_goal.is_negative() {
            return Err("negative goal");
        }
        if self.idle_signout_mins > MAX_IDLE_MINS {
            return Err("idle sign-out too long");
        }
        if self.roles.contains_key(&Role::Manager) {
            return Err("manager permissions are fixed");
        }
        let fixed = Permission::ALL
            .iter()
            .filter(|p| p.manager_only())
            .fold(0, |m, p| m | p.bit());
        if self.roles.values().any(|m| m & fixed != 0) {
            return Err("manager-only permission granted");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_static_matrix() {
        let p = Policy::default();
        for r in Role::ALL {
            for perm in Permission::ALL {
                assert_eq!(p.allows(r, perm), role_allows(r, perm), "{r:?} {perm:?}");
            }
        }
        assert!(p.validate().is_ok());
    }

    #[test]
    fn snapshot_before_policy_still_loads() {
        use crate::state::PosState;
        // A pre-policy snapshot is today's layout without the trailing
        // policy and stats fields.
        let st = PosState {
            next_receipt: 42,
            ..PosState::default()
        };
        let full = postcard::to_allocvec(&st).unwrap();
        let tail = [
            postcard::to_allocvec(&st.policy).unwrap(),
            postcard::to_allocvec(&st.stats).unwrap(),
            postcard::to_allocvec(&st.ui).unwrap(),
            postcard::to_allocvec(&st.wifi).unwrap(),
        ]
        .concat();
        let old = &full[..full.len() - tail.len()];
        let back = PosState::from_snapshot(old).unwrap();
        assert_eq!(back.next_receipt, 42);
        assert_eq!(back.policy, Policy::default());
        assert_eq!(PosState::from_snapshot(&full).unwrap(), st);
    }

    #[test]
    fn toggles_and_fixed_rights() {
        let mut p = Policy::default();
        assert!(!p.allows(Role::Cashier, Permission::Refund));
        assert!(p.set(Role::Cashier, Permission::Refund, true));
        assert!(p.allows(Role::Cashier, Permission::Refund));
        assert!(p.set(Role::Cashier, Permission::Sell, false));
        assert!(!p.allows(Role::Cashier, Permission::Sell));
        assert!(!p.set(Role::Supervisor, Permission::ManageUsers, true));
        assert!(!p.set(Role::Manager, Permission::Sell, false));
        assert!(p.allows(Role::Manager, Permission::Sell));
        let mut bad = p.clone();
        bad.roles.insert(Role::Cashier, u32::MAX);
        assert!(bad.validate().is_err());
        assert!(!bad.allows(Role::Cashier, Permission::Configure));
    }
}
