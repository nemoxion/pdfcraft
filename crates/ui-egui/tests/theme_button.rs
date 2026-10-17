//! The title bar's theme button: a click cycles System, Light and Dark; a right-click opens the
//! menu with the appearance settings. The Dark PDF pages toggle sits beside it.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pdfcraft_ui_egui::PdfCraftApp;

fn harness() -> Harness<'static, PdfCraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(|_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app
    });
    h.run_steps(6);
    h
}

fn preference(h: &Harness<'static, PdfCraftApp>) -> String {
    format!("{:?}", h.state().theme_preference)
}

#[test]
fn a_click_cycles_the_theme_and_a_right_click_opens_the_menu() {
    let mut h = harness();
    assert_eq!(preference(&h), "Light", "new installs start Light");
    for (label, next) in
        [("Display theme: Light gray", "Dark"), ("Display theme: Dark gray", "System"), ("Display theme: Use system setting", "Light")]
    {
        h.get_by_label(label).click();
        h.run_steps(4);
        assert_eq!(preference(&h), next);
    }
    assert!(h.query_by_label("Appearance settings…").is_none(), "a plain click opens no menu");

    h.get_by_label("Display theme: Light gray").click_secondary();
    h.run_steps(4);
    h.get_by_label("Appearance settings…").click();
    h.run_steps(4);
    h.run_steps(10);
    assert!(h.query_by_label("Light theme").is_some(), "Preferences opens on Appearance");
}

fn settle(h: &mut Harness<'static, PdfCraftApp>) {
    for _ in 0..200 {
        h.run_steps(2);
        if !h.state().render_pending() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn dark_pdf_pages_toggle_is_beside_the_theme_button() {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(|_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.open_bytes("form.pdf", None, include_bytes!("data/form.pdf").to_vec()).unwrap();
        app.set_option("left", "closed").unwrap();
        app
    });
    h.run_steps(4);
    settle(&mut h);
    assert!(!h.state().views[0].dark_pages);
    h.get_by_label("Dark PDF pages").click();
    h.run_steps(3);
    assert!(h.state().views[0].dark_pages);
    assert!(h.state().view_defaults.dark_pages, "the choice carries over to newly opened documents");
    settle(&mut h);
    if let Ok(dir) = std::env::var("PDFCRAFT_SHOT") {
        h.render().unwrap().save(format!("{dir}/dark-pdf-pages.png")).unwrap();
    }
    h.get_by_label("Dark PDF pages").click();
    h.run_steps(3);
    assert!(!h.state().views[0].dark_pages);
}
