// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use slint::{Timer, TimerMode};
use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;
use std::time::Duration;

slint::include_modules!();

fn main() -> Result<(), Box<dyn Error>> {
    let ui = AppWindow::new()?;

    let state = Rc::new(RefCell::new(Pomodoro {
        status: Status::Idle,
        mode: Mode::Work,
        remain_seconds: WORK_TIME,
    }));

    let ui_weak = ui.as_weak();

    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_secs(1), {
        let state_clone = state.clone();

        move || {
            let mut state = state_clone.borrow_mut();

            if !matches!(state.status, Status::Running) {
                return;
            }
            if state.remain_seconds > 0 {
                state.remain_seconds -= 1;
            }

            if state.remain_seconds == 0 {
                match state.mode {
                    Mode::Work => {
                        state.mode = Mode::Break;
                        state.remain_seconds = BREAK_TIME;
                    }
                    Mode::Break => {
                        state.mode = Mode::Work;
                        state.remain_seconds = WORK_TIME;
                    }
                }
            }
            drop(state);

            if let Some(ui) = ui_weak.upgrade() {
                let state = state_clone.borrow();
                sync_ui(&ui, &state);
            }
        }
    });

    ui.on_start_clicked({
        let state = state.clone();
        let ui_weak = ui.as_weak();

        move || {
            let status = Status::Running;
            state.borrow_mut().status = status;
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_status(status.to_str().into());
            }
        }
    });

    ui.on_pause_clicked({
        let state = state.clone();
        let ui_weak = ui.as_weak();

        move || {
            let status = Status::Paused;
            state.borrow_mut().status = status;

            if let Some(ui) = ui_weak.upgrade() {
                ui.set_status(status.to_str().into());
            }
        }
    });

    ui.on_reset_clicked({
        let state = state.clone();
        let ui_weak = ui.as_weak();

        move || {
            {
                let mut state = state.borrow_mut();
                state.status = Status::Idle;
                state.mode = Mode::Work;
                state.remain_seconds = WORK_TIME;
            }

            if let Some(ui) = ui_weak.upgrade() {
                sync_ui(&ui, &state.borrow());
            }
        }
    });

    sync_ui(&ui, &state.borrow());

    ui.run()?;

    Ok(())
}

#[derive(Clone, Copy)]
enum Mode {
    Work,
    Break,
}

impl Mode {
    fn to_str(self) -> &'static str {
        match self {
            Mode::Work => "Work",
            Mode::Break => "Break",
        }
    }
}

#[derive(Clone, Copy)]
enum Status {
    Idle,
    Running,
    Paused,
}

impl Status {
    fn to_str(self) -> &'static str {
        match self {
            Status::Idle => "Idle",
            Status::Running => "Running",
            Status::Paused => "Paused",
        }
    }
}

struct Pomodoro {
    //当前状态
    status: Status,
    //当前模式
    mode: Mode,
    //剩余秒数
    remain_seconds: u32,
}

const WORK_TIME: u32 = 25 * 60;
const BREAK_TIME: u32 = 5 * 60;

fn format_time(seconds: u32) -> String {
    let m = seconds / 60;
    let s = seconds % 60;
    format!("{:02}:{:02}", m, s)
}

fn sync_ui(ui: &AppWindow, state: &Pomodoro) {
    ui.set_status(state.status.to_str().into());
    ui.set_mode_text(state.mode.to_str().into());
    ui.set_time_text(format_time(state.remain_seconds).into());
}
