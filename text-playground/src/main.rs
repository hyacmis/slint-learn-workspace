#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod markdown_page;
mod text_page;

use std::error::Error;

slint::include_modules!();

fn main() -> Result<(), Box<dyn Error>> {
    let ui = AppWindow::new()?;

    ui.on_text_changed(|text| text_page::metrics(&text));
    ui.on_transform_clicked(|operation, text| text_page::transform(&text, operation));
    ui.on_replace_clicked(|find, replacement, text| text_page::replace(&text, &find, &replacement));
    ui.on_markdown_changed(|text| markdown_page::changed_message(&text).into());
    ui.on_markdown_operation_clicked(markdown_page::insert_template);

    ui.invoke_initialize();
    ui.run()?;
    Ok(())
}
