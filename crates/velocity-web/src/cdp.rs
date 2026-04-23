use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    MouseButton,
};
use chromiumoxide::Page;
use velocity_common::{Result, VelocityError};

fn cdp_err(e: impl std::fmt::Display) -> VelocityError {
    VelocityError::Internal(anyhow::anyhow!("{e}"))
}

/// Dispatch a left-click (mousePressed + mouseReleased) at `(x, y)`.
pub async fn click(page: &Page, x: f64, y: f64) -> Result<()> {
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;

    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;

    Ok(())
}

/// Dispatch a double-click at `(x, y)`.
pub async fn double_click(page: &Page, x: f64, y: f64) -> Result<()> {
    for click_count in [1u32, 2] {
        page.execute(
            DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MousePressed)
                .x(x)
                .y(y)
                .button(MouseButton::Left)
                .click_count(click_count)
                .build()
                .unwrap(),
        )
        .await
        .map_err(cdp_err)?;
        page.execute(
            DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MouseReleased)
                .x(x)
                .y(y)
                .button(MouseButton::Left)
                .click_count(click_count)
                .build()
                .unwrap(),
        )
        .await
        .map_err(cdp_err)?;
    }
    Ok(())
}

/// Hold mousedown for `duration_ms` then release (long press).
pub async fn long_press(page: &Page, x: f64, y: f64, duration_ms: u64) -> Result<()> {
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;

    tokio::time::sleep(Duration::from_millis(duration_ms)).await;

    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;

    Ok(())
}

/// Simulate a mouse drag from `from` to `to`.
pub async fn drag(page: &Page, from: (i32, i32), to: (i32, i32)) -> Result<()> {
    let (fx, fy) = (from.0 as f64, from.1 as f64);
    let (tx, ty) = (to.0 as f64, to.1 as f64);

    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(fx)
            .y(fy)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;

    // Move in a single step (sufficient for scroll triggers).
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseMoved)
            .x(tx)
            .y(ty)
            .button(MouseButton::Left)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;

    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(tx)
            .y(ty)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;

    Ok(())
}

/// Dispatch a single key event (keyDown + keyUp) by key name.
pub async fn dispatch_key(page: &Page, key: &str) -> Result<()> {
    page.execute(
        DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .key(key)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;

    page.execute(
        DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(key)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;

    Ok(())
}

/// Dispatch Ctrl+A (select all) to the focused element.
pub async fn select_all(page: &Page) -> Result<()> {
    // modifiers: 2 = Control key
    page.execute(
        DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .key("a")
            .modifiers(2i64)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;
    page.execute(
        DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key("a")
            .modifiers(2i64)
            .build()
            .unwrap(),
    )
    .await
    .map_err(cdp_err)?;
    Ok(())
}
