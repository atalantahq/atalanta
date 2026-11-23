// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! The actionability rule for the action running now (D-3), for drivers
//! that check actionability themselves (the web driver's `act`).
//!
//! The executor runs each action under [`with_mode`]; a driver reads
//! [`mode`]. Outside any scope (direct driver calls, MCP, the inspector)
//! the mode is [`ActMode::Lenient`], the behaviour before D-3.

/// How strictly the running action checks its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActMode {
    /// Every check fails the action: still moving, disabled (inherited
    /// `aria-disabled` too), or the click landing on anything but the
    /// target, an ancestor included.
    Strict,
    /// Only the checks that always failed fail; the rest warn.
    Lenient,
    /// `force: true`: no checks; act at the target's centre.
    Force,
}

tokio::task_local! {
    static MODE: ActMode;
}

/// Run `f` with `mode` as the actionability rule of the actions inside it.
pub async fn with_mode<F: std::future::Future>(mode: ActMode, f: F) -> F::Output {
    MODE.scope(mode, f).await
}

/// The actionability rule here (see [`with_mode`]).
pub fn mode() -> ActMode {
    MODE.try_with(|m| *m).unwrap_or(ActMode::Lenient)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_mode_is_scoped_and_lenient_outside() {
        assert_eq!(mode(), ActMode::Lenient);
        assert_eq!(
            with_mode(ActMode::Force, async { mode() }).await,
            ActMode::Force
        );
        assert_eq!(mode(), ActMode::Lenient);
    }
}
