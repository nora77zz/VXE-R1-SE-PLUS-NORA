use tauri::{LogicalSize, PhysicalPosition, WebviewWindow};

const MIN_WINDOW_WIDTH: f64 = 640.0;
const MIN_WINDOW_HEIGHT: f64 = 360.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct InitialWindowLayout {
    width: f64,
    height: f64,
    min_width: f64,
    min_height: f64,
    x: i32,
    y: i32,
}

fn calculate_initial_layout(
    work_x: i32,
    work_y: i32,
    work_width: u32,
    work_height: u32,
    current_width: u32,
    current_height: u32,
    scale_factor: f64,
) -> Option<InitialWindowLayout> {
    if work_width == 0 || work_height == 0 || !scale_factor.is_finite() || scale_factor <= 0.0 {
        return None;
    }

    let available_width = (f64::from(work_width) / scale_factor).max(1.0);
    let available_height = (f64::from(work_height) / scale_factor).max(1.0);
    let min_width = MIN_WINDOW_WIDTH.min(available_width);
    let min_height = MIN_WINDOW_HEIGHT.min(available_height);
    let width = (f64::from(current_width) / scale_factor).clamp(min_width, available_width);
    let height = (f64::from(current_height) / scale_factor).clamp(min_height, available_height);

    let physical_width = (width * scale_factor).round().clamp(1.0, f64::from(work_width)) as i64;
    let physical_height = (height * scale_factor).round().clamp(1.0, f64::from(work_height)) as i64;
    let x = i64::from(work_x) + (i64::from(work_width) - physical_width) / 2;
    let y = i64::from(work_y) + (i64::from(work_height) - physical_height) / 2;

    Some(InitialWindowLayout {
        width,
        height,
        min_width,
        min_height,
        x: x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        y: y.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
    })
}

/// Fits the initial window into the usable work area of its monitor.
///
/// Tauri's window dimensions use logical pixels while monitor work areas are
/// returned in physical pixels, so the monitor scale factor is applied here.
pub fn clamp_main_window(window: &WebviewWindow) -> Result<(), String> {
    let monitor = match window
        .current_monitor()
        .map_err(|error| error.to_string())?
    {
        Some(monitor) => Some(monitor),
        None => match window
            .primary_monitor()
            .map_err(|error| error.to_string())?
        {
            Some(monitor) => Some(monitor),
            None => window
                .available_monitors()
                .map_err(|error| error.to_string())?
                .into_iter()
                .next(),
        },
    };

    let Some(monitor) = monitor else {
        return Ok(());
    };

    let work_area = monitor.work_area();
    let physical_size = window.inner_size().map_err(|error| error.to_string())?;
    let layout = calculate_initial_layout(
        work_area.position.x,
        work_area.position.y,
        work_area.size.width,
        work_area.size.height,
        physical_size.width,
        physical_size.height,
        monitor.scale_factor(),
    )
    .ok_or_else(|| "A área de trabalho do monitor não tem dimensões válidas.".to_owned())?;

    window
        .set_min_size(Some(LogicalSize::new(layout.min_width, layout.min_height)))
        .map_err(|error| error.to_string())?;
    window
        .set_size(LogicalSize::new(layout.width, layout.height))
        .map_err(|error| error.to_string())?;
    window
        .set_position(PhysicalPosition::new(layout.x, layout.y))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::calculate_initial_layout;

    #[test]
    fn shrinks_and_centers_default_window_on_1024_by_560_work_area_at_150_percent() {
        let layout = calculate_initial_layout(0, 0, 1024, 560, 1800, 1140, 1.5).unwrap();

        assert_eq!(layout.min_width, 640.0);
        assert_eq!(layout.min_height, 360.0);
        assert!((layout.width - 1024.0 / 1.5).abs() < 0.01);
        assert!((layout.height - 560.0 / 1.5).abs() < 0.01);
        assert_eq!((layout.x, layout.y), (0, 0));
    }

    #[test]
    fn keeps_default_window_size_when_it_fits_and_centers_in_offset_work_area() {
        let layout = calculate_initial_layout(-1920, 40, 1920, 1040, 1500, 950, 1.25).unwrap();

        assert_eq!((layout.width, layout.height), (1200.0, 760.0));
        assert_eq!((layout.x, layout.y), (-1710, 85));
    }

    #[test]
    fn adapts_minimum_to_work_areas_smaller_than_the_configured_minimum() {
        let layout = calculate_initial_layout(0, 0, 600, 330, 1800, 1140, 1.5).unwrap();

        assert_eq!((layout.min_width, layout.min_height), (400.0, 220.0));
        assert_eq!((layout.width, layout.height), (400.0, 220.0));
        assert_eq!((layout.x, layout.y), (0, 0));
    }

    #[test]
    fn rejects_invalid_scale_and_zero_sized_work_areas() {
        assert!(calculate_initial_layout(0, 0, 1024, 560, 1800, 1140, 0.0).is_none());
        assert!(calculate_initial_layout(0, 0, 0, 560, 1800, 1140, 1.0).is_none());
    }
}
