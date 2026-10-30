//! Parses Valve's radar overview files (assets/overviews/<map>.txt, KeyValues
//! format) into the JSON the radar page uses to line images up with the world:
//!   pos_x / pos_y  world coordinate of the image's top-left corner
//!   scale          world units per image pixel
//!   verticalsections  extra floor images, picked by altitude

use serde_json::{Value, json};

enum Node {
    Str(String),
    Obj(Vec<(String, Node)>),
}

fn tokenize(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = src.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '/' => {
                // `//` comment to end of line
                while chars.next_if(|&c| c != '\n').is_some() {}
            }
            '"' => {
                chars.next();
                let s: String = std::iter::from_fn(|| chars.next_if(|&c| c != '"')).collect();
                chars.next();
                out.push(format!("\"{s}"));
            }
            '{' | '}' => {
                out.push(c.to_string());
                chars.next();
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            _ => {
                let s: String = std::iter::from_fn(|| chars.next_if(|c| !c.is_whitespace() && !"{}\"".contains(*c))).collect();
                out.push(format!("\"{s}"));
            }
        }
    }
    out
}

fn parse_obj(tokens: &mut std::slice::Iter<String>) -> Vec<(String, Node)> {
    let mut items = Vec::new();
    while let Some(tok) = tokens.next() {
        if tok == "}" {
            break;
        }
        let Some(key) = tok.strip_prefix('"') else { continue };
        match tokens.next().map(String::as_str) {
            Some("{") => items.push((key.to_string(), Node::Obj(parse_obj(tokens)))),
            Some(v) => items.push((key.to_string(), Node::Str(v.trim_start_matches('"').to_string()))),
            None => break,
        }
    }
    items
}

fn get<'a>(obj: &'a [(String, Node)], key: &str) -> Option<&'a Node> {
    obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

fn num(obj: &[(String, Node)], key: &str) -> Option<f64> {
    match get(obj, key)? {
        Node::Str(s) => s.parse().ok(),
        Node::Obj(_) => None,
    }
}

/// Builds {"de_nuke": {pos_x, pos_y, scale, sections: [{name, image, min, max}]}, ...}
/// from (url, bytes) assets. Sections whose image is missing are dropped, and
/// maps without a main image are skipped. Also returns the number of maps.
pub fn maps_json(assets: &[(&str, &[u8])]) -> (String, usize) {
    let has = |url: &str| assets.iter().any(|(p, _)| *p == url);
    let mut maps = serde_json::Map::new();

    for (path, bytes) in assets {
        let Some(map) = path.strip_prefix("/assets/overviews/").and_then(|p| p.strip_suffix(".txt")) else {
            continue;
        };
        let tokens = tokenize(&String::from_utf8_lossy(bytes));
        let mut iter = tokens.iter();
        let root = parse_obj(&mut iter);
        let Some((_, Node::Obj(body))) = root.first() else { continue };
        let (Some(pos_x), Some(pos_y), Some(scale)) = (num(body, "pos_x"), num(body, "pos_y"), num(body, "scale")) else {
            continue;
        };

        let main_image = format!("/assets/maps/{map}_radar.png");
        if !has(&main_image) {
            continue;
        }
        let mut sections = vec![json!({ "name": "default", "image": main_image, "min": Value::Null, "max": Value::Null })];
        if let Some(Node::Obj(vs)) = get(body, "verticalsections") {
            sections.clear();
            for (name, node) in vs {
                let Node::Obj(s) = node else { continue };
                let image = if name == "default" {
                    format!("/assets/maps/{map}_radar.png")
                } else {
                    format!("/assets/maps/{map}_{name}_radar.png")
                };
                if has(&image) {
                    sections.push(json!({
                        "name": name, "image": image,
                        "min": num(s, "AltitudeMin"), "max": num(s, "AltitudeMax"),
                    }));
                }
            }
        }
        maps.insert(map.to_string(), json!({ "pos_x": pos_x, "pos_y": pos_y, "scale": scale, "sections": sections }));
    }
    let count = maps.len();
    (Value::Object(maps).to_string(), count)
}
