use std::collections::HashSet;
use std::sync::Arc;
use tracing::{warn, info};

/// Client roles in the system
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Role {
    Sender,
    Receiver,
    Admin,
    Unknown,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Sender => "sender",
            Role::Receiver => "receiver",
            Role::Admin => "admin",
            Role::Unknown => "unknown",
        }
    }
}

/// Result of an authorization check
#[derive(Debug)]
pub enum AuthResult {
    Allowed,
    Denied(&'static str),
}

/// ACL manager: resolves CN → Role and checks command permissions
#[derive(Clone)]
pub struct AclManager {
    sender_cns: Arc<HashSet<String>>,
    receiver_cns: Arc<HashSet<String>>,
    admin_cns: Arc<HashSet<String>>,
}

impl AclManager {
    pub fn new(sender_cns: Vec<String>, receiver_cns: Vec<String>, admin_cns: Vec<String>) -> Self {
        let manager = Self {
            sender_cns: Arc::new(sender_cns.into_iter().collect()),
            receiver_cns: Arc::new(receiver_cns.into_iter().collect()),
            admin_cns: Arc::new(admin_cns.into_iter().collect()),
        };
        info!(
            "ACL initialized: senders={:?}, receivers={:?}, admins={:?}",
            manager.sender_cns, manager.receiver_cns, manager.admin_cns
        );
        manager
    }

    /// Resolve a CN (from mTLS certificate) to a Role
    pub fn resolve_role(&self, cn: &str) -> Role {
        if self.admin_cns.contains(cn) {
            Role::Admin
        } else if self.sender_cns.contains(cn) {
            Role::Sender
        } else if self.receiver_cns.contains(cn) {
            Role::Receiver
        } else {
            warn!(cn = %cn, "Unknown CN — no ACL role assigned");
            Role::Unknown
        }
    }

    /// Check if a command is allowed for a given role.
    ///
    /// # Permission matrix
    ///
    /// | Command             | Sender | Receiver | Admin |
    /// |---------------------|--------|----------|-------|
    /// | declare_queue       |   ❌   |   ✅     |  ✅   |
    /// | declare_exchange    |   ❌   |   ✅     |  ✅   |
    /// | bind                |   ❌   |   ✅     |  ✅   |
    /// | publish             |   ✅   |   ❌     |  ✅   |
    /// | publish_batch       |   ✅   |   ❌     |  ✅   |
    /// | consume             |   ❌   |   ✅     |  ✅   |
    /// | ack                 |   ✅   |   ✅     |  ✅   |
    /// | register_public_key |   ❌   |   ✅     |  ✅   |
    /// | get_public_key      |   ✅   |   ❌     |  ✅   |
    /// | resend              |   ✅   |   ✅     |  ✅   |
    /// | heartbeat           |   ❌   |   ✅     |  ✅   |
    pub fn check_command(&self, role: &Role, command: &str) -> AuthResult {
        // Admin has full access to everything
        if *role == Role::Admin {
            return AuthResult::Allowed;
        }

        // Unknown role: deny everything
        if *role == Role::Unknown {
            return AuthResult::Denied("unknown role — access denied");
        }

        let allowed = match command {
            "declare_queue" => matches!(role, Role::Receiver),
            "declare_exchange" => matches!(role, Role::Receiver),
            "bind" => matches!(role, Role::Receiver),
            "publish" | "publish_batch" => matches!(role, Role::Sender),
            "consume" => matches!(role, Role::Receiver),
            "ack" => matches!(role, Role::Sender | Role::Receiver),
            "register_public_key" => matches!(role, Role::Receiver),
            "get_public_key" => matches!(role, Role::Sender),
            "resend" => matches!(role, Role::Sender | Role::Receiver),
            "heartbeat" => matches!(role, Role::Receiver),
            _ => false,
        };

        if allowed {
            AuthResult::Allowed
        } else {
            AuthResult::Denied("command not permitted for this role")
        }
    }

    /// Check if a client can access a specific resource (e.g. queue/exchange).
    /// For receivers, they can only declare/bind resources matching their own CN prefix.
    pub fn check_resource(&self, role: &Role, cn: &str, resource_type: &str, resource_name: &str) -> AuthResult {
        if *role == Role::Admin {
            return AuthResult::Allowed;
        }

        if *role == Role::Unknown {
            return AuthResult::Denied("unknown role — access denied");
        }

        // Receivers can only manage resources that belong to them
        // Convention: queue = "{cn}_queue", exchange = "{exchange_name}", routing_key = "{cn}_key"
        if *role == Role::Receiver {
            let allowed = match resource_type {
                "queue" => resource_name.starts_with(&format!("{}_", cn)) || resource_name == format!("{}_queue", cn),
                "exchange" => true, // receivers can declare exchanges
                "routing_key" => resource_name.starts_with(cn),
                _ => false,
            };

            if allowed {
                AuthResult::Allowed
            } else {
                warn!(
                    cn = %cn,
                    resource_type = %resource_type,
                    resource_name = %resource_name,
                    "Receiver attempting to access resource outside their scope"
                );
                AuthResult::Denied("resource access denied for this role")
            }
        } else {
            // Sender: should not reach here for declare/bind (command-level check blocks it)
            AuthResult::Denied("resource access denied for this role")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_acl() -> AclManager {
        AclManager::new(
            vec!["sender_1".to_string()],
            vec!["receiver_1".to_string()],
            vec!["admin".to_string()],
        )
    }

    #[test]
    fn test_resolve_role() {
        let acl = make_test_acl();
        assert_eq!(acl.resolve_role("sender_1"), Role::Sender);
        assert_eq!(acl.resolve_role("receiver_1"), Role::Receiver);
        assert_eq!(acl.resolve_role("admin"), Role::Admin);
        assert_eq!(acl.resolve_role("unknown_cn"), Role::Unknown);
    }

    #[test]
    fn test_sender_permissions() {
        let acl = make_test_acl();
        let role = Role::Sender;

        assert!(matches!(acl.check_command(&role, "publish"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "publish_batch"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "get_public_key"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "resend"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "ack"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "declare_queue"), AuthResult::Denied(_)));
        assert!(matches!(acl.check_command(&role, "declare_exchange"), AuthResult::Denied(_)));
        assert!(matches!(acl.check_command(&role, "bind"), AuthResult::Denied(_)));
        assert!(matches!(acl.check_command(&role, "consume"), AuthResult::Denied(_)));
        assert!(matches!(acl.check_command(&role, "register_public_key"), AuthResult::Denied(_)));
    }

    #[test]
    fn test_receiver_permissions() {
        let acl = make_test_acl();
        let role = Role::Receiver;

        assert!(matches!(acl.check_command(&role, "declare_queue"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "declare_exchange"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "bind"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "consume"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "ack"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "register_public_key"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "resend"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "heartbeat"), AuthResult::Allowed));
        assert!(matches!(acl.check_command(&role, "publish"), AuthResult::Denied(_)));
        assert!(matches!(acl.check_command(&role, "publish_batch"), AuthResult::Denied(_)));
        assert!(matches!(acl.check_command(&role, "get_public_key"), AuthResult::Denied(_)));
    }

    #[test]
    fn test_admin_permissions() {
        let acl = make_test_acl();
        let role = Role::Admin;

        // Admin can do everything
        for cmd in &["declare_queue", "declare_exchange", "bind", "publish", "publish_batch",
                      "consume", "ack", "register_public_key", "get_public_key", "resend", "heartbeat"] {
            assert!(matches!(acl.check_command(&role, cmd), AuthResult::Allowed),
                "Admin should be allowed to use command: {}", cmd);
        }
    }

    #[test]
    fn test_unknown_role_denied() {
        let acl = make_test_acl();
        let role = Role::Unknown;

        for cmd in &["publish", "declare_queue", "consume", "heartbeat"] {
            assert!(matches!(acl.check_command(&role, cmd), AuthResult::Denied(_)),
                "Unknown role should be denied command: {}", cmd);
        }
    }

    #[test]
    fn test_receiver_resource_check() {
        let acl = make_test_acl();
        let role = Role::Receiver;

        assert!(matches!(acl.check_resource(&role, "receiver_1", "queue", "receiver_1_queue"), AuthResult::Allowed));
        assert!(matches!(acl.check_resource(&role, "receiver_1", "routing_key", "receiver_1_key"), AuthResult::Allowed));
        assert!(matches!(acl.check_resource(&role, "receiver_1", "exchange", "ciphermq_exchange"), AuthResult::Allowed));
        // Cannot declare another receiver's queue
        assert!(matches!(acl.check_resource(&role, "receiver_1", "queue", "receiver_2_queue"), AuthResult::Denied(_)));
    }

    #[test]
    fn test_admin_resource_check() {
        let acl = make_test_acl();
        let role = Role::Admin;

        assert!(matches!(acl.check_resource(&role, "admin", "queue", "any_queue"), AuthResult::Allowed));
        assert!(matches!(acl.check_resource(&role, "admin", "exchange", "any_exchange"), AuthResult::Allowed));
    }
}
