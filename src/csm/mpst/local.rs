//! The Multiparty Session Types **local type**: one role's view of a protocol,
//! obtained by projecting a [`GlobalType`] onto that role (`super::project`).
//! Compiling a local type yields the role's [`LocalMachine`] (`crate::csm::machine`).
//!
//! [`GlobalType`]: super::global::GlobalType
//! [`LocalMachine`]: crate::csm::machine::LocalMachine
//!
//! Adjacent serde tagging for the same reason as [`GlobalType`] (ADR-006): this
//! enum is recursive.

use serde::{Deserialize, Serialize};

use crate::csm::mpst::global::TypeVar;
use crate::csm::role::{Label, Role};

/// One role's local protocol type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")] // ADR-006: ADJACENT
pub enum LocalType {
    /// `!to⟨label⟩ . cont` — send `label` to `to`.
    Send {
        to: Role,
        label: Label,
        cont: Box<LocalType>,
    },
    /// `?from⟨label⟩ . cont` — receive `label` from `from`.
    Recv {
        from: Role,
        label: Label,
        cont: Box<LocalType>,
    },
    /// `⊕to{ labelᵢ : Lᵢ }` — internal choice: this role *selects* one label to
    /// send to `to` (the projection of a `Choice` onto its sender).
    Select {
        to: Role,
        branches: Vec<LocalBranch>,
    },
    /// `&from{ labelᵢ : Lᵢ }` — external choice: this role *offers* / branches on
    /// the label received from `from` (the projection of a `Choice` onto its
    /// receiver, or the merge of a bystander's branch continuations).
    Branch {
        from: Role,
        branches: Vec<LocalBranch>,
    },
    /// `μ var. body`
    Rec { var: TypeVar, body: Box<LocalType> },
    /// `var`
    Var { var: TypeVar },
    /// `end`
    End,
}

/// One arm of a [`LocalType::Select`] or [`LocalType::Branch`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalBranch {
    pub label: Label,
    pub cont: LocalType,
}

impl LocalType {
    pub fn send(to: impl Into<Role>, label: Label, cont: LocalType) -> Self {
        LocalType::Send {
            to: to.into(),
            label,
            cont: Box::new(cont),
        }
    }
    pub fn recv(from: impl Into<Role>, label: Label, cont: LocalType) -> Self {
        LocalType::Recv {
            from: from.into(),
            label,
            cont: Box::new(cont),
        }
    }
    pub fn select(to: impl Into<Role>, branches: Vec<LocalBranch>) -> Self {
        LocalType::Select {
            to: to.into(),
            branches,
        }
    }
    pub fn branch(from: impl Into<Role>, branches: Vec<LocalBranch>) -> Self {
        LocalType::Branch {
            from: from.into(),
            branches,
        }
    }
    pub fn rec(var: impl Into<TypeVar>, body: LocalType) -> Self {
        LocalType::Rec {
            var: var.into(),
            body: Box::new(body),
        }
    }
    pub fn var(var: impl Into<TypeVar>) -> Self {
        LocalType::Var { var: var.into() }
    }
}

/// One `label : cont` arm.
pub fn lbranch(label: Label, cont: LocalType) -> LocalBranch {
    LocalBranch { label, cont }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_type_recursive_round_trips() {
        let l = LocalType::rec(
            "t",
            LocalType::recv("O", Label::text("ping"), LocalType::var("t")),
        );
        let json = serde_json::to_string(&l).expect("serialize");
        assert!(
            json.contains(r#""type":"rec""#),
            "adjacent tag expected: {json}"
        );
        let back: LocalType = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, l);
    }
}
