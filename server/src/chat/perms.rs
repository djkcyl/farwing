//! Permission computation (`docs/protocol/chat-design.md` §3.2).

use farwing_protocol::chat::{Id, Overwrite, perm};

/// What a member's permissions are computed from.
pub struct Who<'a> {
    pub owner: bool,
    /// The member's roles (not `@everyone`), as `(id, permissions)`.
    pub roles: &'a [(i64, i64)],
    pub muted: bool,
}

/// The group-wide permissions: `@everyone` and the member's roles.
pub fn base(everyone: i64, who: &Who<'_>, frozen: bool) -> i64 {
    let mut p = if who.owner {
        perm::ALL
    } else {
        let p = who.roles.iter().fold(everyone, |p, (_, r)| p | r);
        if p & perm::ADMINISTRATOR != 0 { perm::ALL } else { p }
    };
    if who.muted && p & perm::ADMINISTRATOR == 0 {
        p &= !perm::MUTED;
    }
    if frozen {
        p &= perm::FROZEN;
    }
    p
}

/// The permissions in a channel: `base` with the overwrites of its
/// category, then its own, applied in turn (`@everyone`'s deny and allow,
/// then the deny and allow of the member's roles together).
pub fn channel(
    group_id: i64,
    everyone: i64,
    who: &Who<'_>,
    frozen: bool,
    layers: &[&[Overwrite]],
) -> i64 {
    let full = base(everyone, who, false);
    let mut p = full;
    if full & perm::ADMINISTRATOR == 0 {
        for layer in layers {
            if let Some(o) = layer.iter().find(|o| o.role_id == Id(group_id)) {
                p &= !(o.deny & perm::CHANNEL);
                p |= o.allow & perm::CHANNEL;
            }
            let (mut deny, mut allow) = (0, 0);
            for o in layer.iter() {
                if who.roles.iter().any(|(id, _)| Id(*id) == o.role_id) {
                    deny |= o.deny;
                    allow |= o.allow;
                }
            }
            p &= !(deny & perm::CHANNEL);
            p |= allow & perm::CHANNEL;
        }
        if p & perm::VIEW == 0 {
            return 0;
        }
        if who.muted {
            p &= !perm::MUTED;
        }
    }
    if frozen {
        p &= perm::FROZEN;
    }
    p
}

/// A direct-message side: everything while friends, reading after.
pub fn dm(friends: bool) -> i64 {
    if friends {
        perm::DM
    } else {
        perm::VIEW | perm::READ_HISTORY
    }
}

/// The highest role position of a member (the owner ranks above all).
pub fn rank(owner: bool, positions: impl IntoIterator<Item = i32>) -> i32 {
    if owner {
        i32::MAX
    } else {
        positions.into_iter().max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: i64 = 1000;
    const MOD: i64 = 7;
    const MUTED_ROLE: i64 = 8;

    fn who(roles: &[(i64, i64)]) -> Who<'_> {
        Who {
            owner: false,
            roles,
            muted: false,
        }
    }

    #[test]
    fn roles_add_to_everyone() {
        let roles = [(MOD, perm::KICK | perm::MANAGE_MESSAGES)];
        let p = base(perm::EVERYONE, &who(&roles), false);
        assert_eq!(p, perm::EVERYONE | perm::KICK | perm::MANAGE_MESSAGES);
        let admin = [(MOD, perm::ADMINISTRATOR)];
        assert_eq!(base(perm::EVERYONE, &who(&admin), false), perm::ALL);
    }

    #[test]
    fn overwrites_apply_category_then_channel() {
        // The category hides itself from everyone but moderators; the
        // channel lets everyone read again but not send.
        let category = [
            Overwrite { role_id: Id(G), allow: 0, deny: perm::VIEW },
            Overwrite { role_id: Id(MOD), allow: perm::VIEW, deny: 0 },
        ];
        let own = [Overwrite { role_id: Id(G), allow: perm::VIEW, deny: perm::SEND }];
        let member = who(&[]);
        assert_eq!(channel(G, perm::EVERYONE, &member, false, &[&category]), 0);
        let p = channel(G, perm::EVERYONE, &member, false, &[&category, &own]);
        assert!(p & perm::VIEW != 0 && p & perm::SEND == 0);
        let roles = [(MOD, 0)];
        let moderator = who(&roles);
        let p = channel(G, perm::EVERYONE, &moderator, false, &[&category]);
        assert!(p & perm::VIEW != 0 && p & perm::SEND != 0);
    }

    #[test]
    fn role_allow_beats_role_deny_and_everyone() {
        let layer = [
            Overwrite { role_id: Id(G), allow: 0, deny: perm::SEND },
            Overwrite { role_id: Id(MUTED_ROLE), allow: 0, deny: perm::SEND },
            Overwrite { role_id: Id(MOD), allow: perm::SEND, deny: 0 },
        ];
        let both = [(MOD, 0), (MUTED_ROLE, 0)];
        assert!(channel(G, perm::EVERYONE, &who(&both), false, &[&layer]) & perm::SEND != 0);
        let muted_role = [(MUTED_ROLE, 0)];
        assert!(channel(G, perm::EVERYONE, &who(&muted_role), false, &[&layer]) & perm::SEND == 0);
    }

    #[test]
    fn group_bits_are_not_per_channel() {
        let layer = [Overwrite { role_id: Id(G), allow: perm::BAN | perm::MANAGE_ROLES, deny: 0 }];
        let p = channel(G, perm::EVERYONE, &who(&[]), false, &[&layer]);
        assert_eq!(p & (perm::BAN | perm::MANAGE_ROLES), 0);
    }

    #[test]
    fn administrators_and_owners_ignore_overwrites() {
        let layer = [Overwrite { role_id: Id(G), allow: 0, deny: perm::VIEW }];
        let admin = [(MOD, perm::ADMINISTRATOR)];
        assert_eq!(channel(G, perm::EVERYONE, &who(&admin), false, &[&layer]), perm::ALL);
        let owner = Who { owner: true, roles: &[], muted: false };
        assert_eq!(channel(G, perm::EVERYONE, &owner, false, &[&layer]), perm::ALL);
    }

    #[test]
    fn muted_and_frozen() {
        let muted = Who { owner: false, roles: &[], muted: true };
        let p = channel(G, perm::EVERYONE, &muted, false, &[]);
        assert!(p & perm::VIEW != 0 && p & (perm::SEND | perm::ATTACH | perm::SPEAK) == 0);
        let p = channel(G, perm::EVERYONE, &who(&[]), true, &[]);
        assert_eq!(p, perm::VIEW | perm::READ_HISTORY);
        let owner = Who { owner: true, roles: &[], muted: false };
        assert_eq!(channel(G, perm::EVERYONE, &owner, true, &[]), perm::VIEW | perm::READ_HISTORY);
    }

    #[test]
    fn direct_messages_turn_read_only() {
        assert!(dm(true) & perm::SEND != 0);
        assert_eq!(dm(false), perm::VIEW | perm::READ_HISTORY);
    }
}
