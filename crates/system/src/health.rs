//! Config health: evaluation errors, options set in several places and binds
//! replaced by later binds or unbinds.

use hyprdeck_core::hypr::model::{BindEntry, ConfigModel, OptionEntry, UnbindEntry};

/// An option assigned more than once; the last assignment wins.
#[derive(Debug)]
pub struct OptionOverride<'a> {
    pub key: &'a str,
    pub winner: &'a OptionEntry,
    /// Earlier assignments, in config order.
    pub overridden: Vec<&'a OptionEntry>,
}

#[derive(Debug)]
pub enum BindFate<'a> {
    /// Removed by a later `hl.unbind`; `replacement` is the bind now active on the combo.
    Unbound {
        by: &'a UnbindEntry,
        replacement: Option<&'a BindEntry>,
    },
    /// Still active, but a later bind on the same keys runs as well.
    AlsoBound { by: &'a BindEntry },
}

#[derive(Debug)]
pub struct BindOverride<'a> {
    pub bind: &'a BindEntry,
    pub fate: BindFate<'a>,
}

pub fn option_overrides(m: &ConfigModel) -> Vec<OptionOverride<'_>> {
    let mut by_key: std::collections::BTreeMap<&str, Vec<&OptionEntry>> = Default::default();
    for o in &m.options {
        by_key.entry(&o.key).or_default().push(o);
    }
    by_key
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(key, mut v)| {
            v.sort_by_key(|o| o.seq);
            let winner = v.pop().expect("len > 1");
            OptionOverride {
                key,
                winner,
                overridden: v,
            }
        })
        .collect()
}

pub fn bind_overrides(m: &ConfigModel) -> Vec<BindOverride<'_>> {
    let effective = m.effective_binds();
    let mut out = Vec::new();
    for b in &m.binds {
        let same = |submap: &str, combo| submap == b.submap && b.combo.matches(combo);
        let unbind = m
            .unbinds
            .iter()
            .filter(|u| u.seq > b.seq && b.combo.matches(&u.combo))
            .min_by_key(|u| u.seq);
        if let Some(by) = unbind {
            let replacement = effective
                .iter()
                .copied()
                .filter(|e| same(&e.submap, &e.combo))
                .max_by_key(|e| e.seq);
            out.push(BindOverride {
                bind: b,
                fate: BindFate::Unbound { by, replacement },
            });
        } else if let Some(by) = m
            .binds
            .iter()
            .filter(|o| o.seq > b.seq && same(&o.submap, &o.combo))
            .min_by_key(|o| o.seq)
        {
            out.push(BindOverride {
                bind: b,
                fate: BindFate::AlsoBound { by },
            });
        }
    }
    out
}

/// Short text of what a bind does.
pub fn bind_action(b: &BindEntry) -> &str {
    b.exec.as_deref().unwrap_or(&b.action_lua)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use hyprdeck_core::hypr::model::{Combo, Source, Value};

    use super::*;

    fn src(line: u32) -> Source {
        Source {
            file: PathBuf::from("/x.lua"),
            line,
        }
    }

    fn bind(seq: u64, keys: &str, exec: &str) -> BindEntry {
        BindEntry {
            seq,
            keys: keys.into(),
            combo: Combo::parse(keys),
            dispatcher: Some("exec_cmd".into()),
            action_lua: format!("hl.dsp.exec_cmd(\"{exec}\")"),
            exec: Some(exec.into()),
            opts_lua: String::new(),
            description: None,
            submap: String::new(),
            source: src(seq as u32),
        }
    }

    #[test]
    fn finds_option_and_bind_overrides() {
        let m = ConfigModel {
            options: vec![
                OptionEntry {
                    seq: 1,
                    key: "misc.vrr".into(),
                    value: Value::Int(3),
                    source: src(1),
                },
                OptionEntry {
                    seq: 2,
                    key: "input.follow_mouse".into(),
                    value: Value::Int(1),
                    source: src(2),
                },
                OptionEntry {
                    seq: 9,
                    key: "misc.vrr".into(),
                    value: Value::Int(0),
                    source: src(9),
                },
            ],
            binds: vec![
                bind(3, "SUPER + W", "firefox"),
                bind(4, "SUPER + E", "dolphin"),
                bind(5, "super+e", "thunar"),
                bind(11, "SUPER + W", "chromium"),
            ],
            unbinds: vec![UnbindEntry {
                seq: 10,
                keys: "SUPER + W".into(),
                combo: Combo::parse("SUPER + W"),
                source: src(10),
            }],
            ..Default::default()
        };
        let o = option_overrides(&m);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].key, "misc.vrr");
        assert_eq!(o[0].winner.value, Value::Int(0));
        assert_eq!(o[0].overridden.len(), 1);

        let b = bind_overrides(&m);
        assert_eq!(b.len(), 2);
        match &b[0].fate {
            BindFate::Unbound { by, replacement } => {
                assert_eq!(b[0].bind.seq, 3);
                assert_eq!(by.seq, 10);
                assert_eq!(replacement.map(|r| r.seq), Some(11));
            }
            other => panic!("{other:?}"),
        }
        match &b[1].fate {
            BindFate::AlsoBound { by } => assert_eq!((b[1].bind.seq, by.seq), (4, 5)),
            other => panic!("{other:?}"),
        }
    }
}
