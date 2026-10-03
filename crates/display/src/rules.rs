//! Swapping managed monitor rules in and out so a trial apply can be undone
//! exactly, touching only the outputs that were changed (other hyprdeck
//! pages and processes edit the same managed state).

use hyprdeck_core::hypr::managed::{Managed, MonitorRule};

/// What an output's managed rule looked like before [`swap`].
#[derive(Debug, Clone, PartialEq)]
pub struct Restore {
    pub output: String,
    /// Index and rule, `None` when there was no managed rule.
    pub prev: Option<(usize, MonitorRule)>,
}

/// Replace (`Some`) or remove (`None`) the managed rule for `output`.
pub fn swap(m: &mut Managed, output: &str, next: Option<MonitorRule>) -> Restore {
    let pos = m.monitors.iter().position(|r| r.output == output);
    let prev = pos.map(|i| (i, m.monitors[i].clone()));
    match (pos, next) {
        (Some(i), Some(rule)) => m.monitors[i] = rule,
        (Some(i), None) => {
            m.monitors.remove(i);
        }
        (None, Some(rule)) => m.monitors.push(rule),
        (None, None) => {}
    }
    Restore {
        output: output.to_owned(),
        prev,
    }
}

/// Put back what [`swap`] replaced, at its original position.
pub fn restore(m: &mut Managed, r: &Restore) {
    let pos = m.monitors.iter().position(|x| x.output == r.output);
    match (pos, &r.prev) {
        (Some(i), Some((_, rule))) => m.monitors[i] = rule.clone(),
        (Some(i), None) => {
            m.monitors.remove(i);
        }
        (None, Some((at, rule))) => m.monitors.insert((*at).min(m.monitors.len()), rule.clone()),
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use hyprdeck_core::hypr::managed::OptValue;

    use super::*;

    fn rule(output: &str, mode: &str) -> MonitorRule {
        MonitorRule {
            output: output.into(),
            mode: Some(mode.into()),
            ..Default::default()
        }
    }

    fn base() -> Managed {
        let mut m = Managed::default();
        m.options.insert("misc.vrr".into(), OptValue::Int(0));
        m.monitors = vec![
            rule("DP-1", "1920x1080@60.00Hz"),
            rule("HDMI-A-1", "3840x2160@119.88Hz"),
            rule("DP-2", "preferred"),
        ];
        m
    }

    #[test]
    fn swap_and_restore_roundtrip() {
        let orig = base();
        for (output, next) in [
            ("HDMI-A-1", Some(rule("HDMI-A-1", "3840x2160@60.00Hz"))),
            ("HDMI-A-1", None),
            ("DP-3", Some(rule("DP-3", "preferred"))),
            ("DP-3", None),
        ] {
            let mut m = orig.clone();
            let r = swap(&mut m, output, next.clone());
            assert_eq!(
                m.monitors.iter().find(|x| x.output == output),
                next.as_ref()
            );
            restore(&mut m, &r);
            assert_eq!(m, orig, "{output} {next:?}");
        }
    }

    #[test]
    fn restore_only_touches_its_output() {
        let mut m = base();
        let r = swap(&mut m, "HDMI-A-1", None);
        // Another writer changes unrelated state meanwhile.
        m.options
            .insert("input.follow_mouse".into(), OptValue::Int(2));
        m.monitors[0].scale = Some(2.0);
        restore(&mut m, &r);
        assert_eq!(m.monitors[1], rule("HDMI-A-1", "3840x2160@119.88Hz"));
        assert_eq!(m.monitors[0].scale, Some(2.0));
        assert_eq!(m.options.len(), 2);
    }
}
