//! Canvas clamping and the field/text arena round-trip.

use scia_scenes::{Canvas, Primitive, Style};

#[test]
fn canvas_clamps_input() {
    let mut c = Canvas::new(1.0);

    // Out-of-range and NaN coordinates/sizes, an out-of-range slot and an
    // out-of-range intensity.
    c.bar(-1.0, 2.0, f32::NAN, 3.0, Style::new(200, 5.0));
    c.line(f32::NAN, -0.5, 2.0, 0.5, 9.0, Style::new(9, f32::NAN));
    c.point(2.0, -1.0, f32::INFINITY, Style::new(3, -2.0));

    match c.primitives()[0] {
        Primitive::Bar { x, y, w, h, style } => {
            assert_eq!(x, 0.0, "x clamped from -1.0");
            assert_eq!(y, 1.0, "y clamped from 2.0");
            assert_eq!(w, 0.0, "NaN w clamped to 0.0");
            assert_eq!(h, 1.0, "h clamped from 3.0");
            assert_eq!(style.slot, 7, "slot clamped from 200 to 7");
            assert_eq!(style.intensity, 1.0, "intensity clamped from 5.0");
        }
        other => panic!("expected Bar, got {other:?}"),
    }

    match c.primitives()[1] {
        Primitive::Line {
            x0,
            y0,
            x1,
            width,
            style,
            ..
        } => {
            assert_eq!(x0, 0.0, "NaN x0 clamped to 0.0");
            assert_eq!(y0, 0.0, "y0 clamped from -0.5");
            assert_eq!(x1, 1.0, "x1 clamped from 2.0");
            assert_eq!(width, 1.0, "width clamped from 9.0");
            assert_eq!(style.intensity, 0.0, "NaN intensity clamped to 0.0");
        }
        other => panic!("expected Line, got {other:?}"),
    }

    match c.primitives()[2] {
        Primitive::Point { x, y, size, style } => {
            assert_eq!(x, 1.0, "x clamped from 2.0");
            assert_eq!(y, 0.0, "y clamped from -1.0");
            assert_eq!(size, 1.0, "infinite size clamped to 1.0");
            assert_eq!(style.intensity, 0.0, "intensity clamped from -2.0");
        }
        other => panic!("expected Point, got {other:?}"),
    }
}

#[test]
fn field_clamps_values_and_pads_short_input() {
    let mut c = Canvas::new(1.0);
    // Three values for a 2x2 field: out of range, NaN, in range, and a missing
    // fourth that must pad to 0.0.
    c.field(2, 2, &[2.0, f32::NAN, 0.4], Style::new(1, 0.5));
    let p = c.primitives()[0];
    let data = c.field_of(&p).expect("field values");
    assert_eq!(data, &[1.0, 0.0, 0.4, 0.0]);
}

#[test]
fn field_and_text_round_trip() {
    let mut c = Canvas::new(1.0);

    let f1 = [0.1f32, 0.2, 0.3, 0.4];
    let f2 = [0.5f32, 0.6];
    c.field(2, 2, &f1, Style::new(0, 1.0));
    c.text(0.1, 0.2, "hello", Style::new(1, 0.5));
    c.bar(0.0, 0.0, 0.5, 0.5, Style::new(2, 0.3));
    c.text(0.3, 0.4, "world!", Style::new(3, 0.9));
    c.field(1, 2, &f2, Style::new(4, 0.2));

    let prims: Vec<Primitive> = c.primitives().to_vec();

    assert_eq!(c.field_of(&prims[0]), Some(&f1[..]));
    assert_eq!(c.text_of(&prims[1]), Some("hello"));
    // Wrong-kind lookups return None.
    assert_eq!(c.field_of(&prims[1]), None);
    assert_eq!(c.text_of(&prims[0]), None);
    // A bar carries neither.
    assert_eq!(c.field_of(&prims[2]), None);
    assert_eq!(c.text_of(&prims[2]), None);
    assert_eq!(c.text_of(&prims[3]), Some("world!"));
    assert_eq!(c.field_of(&prims[4]), Some(&f2[..]));

    // The arenas hold every run back to back.
    assert_eq!(c.field_data(), &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
    assert_eq!(c.text_data(), "helloworld!");
}

#[test]
fn clear_retains_and_empties() {
    let mut c = Canvas::new(1.0);
    c.bar(0.0, 0.0, 1.0, 1.0, Style::new(0, 1.0));
    c.field(2, 1, &[0.2, 0.3], Style::new(0, 1.0));
    c.text(0.0, 0.0, "x", Style::new(0, 1.0));
    c.clear();
    assert!(c.primitives().is_empty());
    assert!(c.field_data().is_empty());
    assert!(c.text_data().is_empty());
}
