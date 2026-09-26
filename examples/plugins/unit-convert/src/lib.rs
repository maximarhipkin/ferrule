//! Pure compute: no capabilities at all. Converts length, mass and
//! temperature units.

use ferrule_plugin_sdk::{export, json, Value};

fn call(tool: &str, args: Value) -> Result<Value, String> {
    match tool {
        "convert" => {
            let value = args["value"].as_f64().ok_or("`value` must be a number")?;
            let from = args["from"].as_str().ok_or("`from` is required")?;
            let to = args["to"].as_str().ok_or("`to` is required")?;
            let out = convert(value, from, to)?;
            Ok(json!(format!("{value} {from} = {} {to}", round(out))))
        }
        "units" => Ok(json!({
            "length": LENGTH.iter().map(|u| u.0).collect::<Vec<_>>(),
            "mass": MASS.iter().map(|u| u.0).collect::<Vec<_>>(),
            "temperature": ["c", "f", "k"],
        })),
        other => Err(format!("no tool `{other}`")),
    }
}

export!(call);

/// Each unit in its base (metre, kilogram).
const LENGTH: &[(&str, f64)] = &[
    ("mm", 0.001),
    ("cm", 0.01),
    ("m", 1.0),
    ("km", 1000.0),
    ("in", 0.0254),
    ("ft", 0.3048),
    ("yd", 0.9144),
    ("mi", 1609.344),
];
const MASS: &[(&str, f64)] = &[
    ("g", 0.001),
    ("kg", 1.0),
    ("t", 1000.0),
    ("oz", 0.028_349_523_125),
    ("lb", 0.453_592_37),
];

fn convert(value: f64, from: &str, to: &str) -> Result<f64, String> {
    let (from, to) = (from.to_ascii_lowercase(), to.to_ascii_lowercase());
    for table in [LENGTH, MASS] {
        let find = |u: &str| table.iter().find(|x| x.0 == u).map(|x| x.1);
        match (find(&from), find(&to)) {
            (Some(a), Some(b)) => return Ok(value * a / b),
            (None, None) => continue,
            _ => return Err(format!("can't convert {from} to {to}")),
        }
    }
    let kelvin = match from.as_str() {
        "c" => value + 273.15,
        "f" => (value - 32.0) * 5.0 / 9.0 + 273.15,
        "k" => value,
        _ => return Err(format!("unknown unit `{from}` (the `units` tool lists them)")),
    };
    match to.as_str() {
        "c" => Ok(kelvin - 273.15),
        "f" => Ok((kelvin - 273.15) * 9.0 / 5.0 + 32.0),
        "k" => Ok(kelvin),
        _ => Err(format!("can't convert {from} to {to}")),
    }
}

fn round(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts() {
        assert_eq!(round(convert(1.0, "mi", "km").unwrap()), 1.609344);
        assert_eq!(round(convert(100.0, "c", "f").unwrap()), 212.0);
        assert!(convert(1.0, "kg", "m").is_err());
    }
}
