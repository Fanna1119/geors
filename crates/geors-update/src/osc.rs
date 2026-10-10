//! Streaming parser for OsmChange (`.osc`, optionally gzip) files.

use std::io::BufRead;

use anyhow::{Context, Result, anyhow, bail};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::osm::{ChangeSet, Elem, Kind, Member, Tags};

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Upsert,
    Delete,
}

fn attr(e: &BytesStart, name: &str) -> Result<Option<String>> {
    for a in e.attributes() {
        let a = a?;
        if a.key.as_ref() == name {
            return Ok(Some(
                a.normalized_value(XmlVersion::Implicit1_0)?.into_owned(),
            ));
        }
    }
    Ok(None)
}

fn req(e: &BytesStart, name: &str) -> Result<String> {
    attr(e, name)?.ok_or_else(|| anyhow!("<{}> without '{name}'", e.name().as_ref()))
}

/// "47.1234567" -> 471234567 (exact for the 7 decimals OSM uses).
fn e7(s: &str) -> Result<i32> {
    let v: f64 = s.parse().with_context(|| format!("bad coordinate '{s}'"))?;
    Ok((v * 1e7).round() as i32)
}

fn kind_of(name: &str) -> Option<Kind> {
    match name {
        "node" => Some(Kind::Node),
        "way" => Some(Kind::Way),
        "relation" => Some(Kind::Relation),
        _ => None,
    }
}

/// An element being read: header attributes plus children so far.
struct Open {
    kind: Kind,
    id: i64,
    lon: i32,
    lat: i32,
    tags: Tags,
    refs: Vec<i64>,
    members: Vec<Member>,
}

impl Open {
    fn finish(self) -> Elem {
        match self.kind {
            Kind::Node => Elem::Node {
                id: self.id,
                lon: self.lon,
                lat: self.lat,
                tags: self.tags,
            },
            Kind::Way => Elem::Way {
                id: self.id,
                refs: self.refs,
                tags: self.tags,
            },
            Kind::Relation => Elem::Relation {
                id: self.id,
                members: self.members,
                tags: self.tags,
            },
        }
    }
}

/// Apply one OsmChange document to `changes` (later calls override).
/// Returns the number of element changes read.
pub fn apply(reader: impl BufRead, changes: &mut ChangeSet) -> Result<usize> {
    let mut xml = Reader::from_reader(reader);
    let mut buf = Vec::new();
    let mut action = None;
    let mut open: Option<Open> = None;
    let mut count = 0;
    loop {
        let ev = xml
            .read_event_into(&mut buf)
            .with_context(|| format!("invalid XML at byte {}", xml.buffer_position()))?;
        let (e, empty) = match &ev {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::End(e) => {
                if let (Some(_), Some(_)) = (kind_of(e.name().as_ref()), &open) {
                    count += 1;
                    finish(open.take().unwrap(), action, changes)?;
                } else if matches!(e.name().as_ref(), "create" | "modify" | "delete") {
                    action = None;
                }
                buf.clear();
                continue;
            }
            Event::Eof => break,
            _ => {
                buf.clear();
                continue;
            }
        };
        let name = e.name();
        match name.as_ref() {
            "create" | "modify" => action = Some(Action::Upsert),
            "delete" => action = Some(Action::Delete),
            n if kind_of(n).is_some() => {
                let kind = kind_of(n).unwrap();
                let id: i64 = req(e, "id")?.parse().context("bad element id")?;
                let (lon, lat) = match (kind, action) {
                    (Kind::Node, Some(Action::Upsert)) => {
                        (e7(&req(e, "lon")?)?, e7(&req(e, "lat")?)?)
                    }
                    _ => (0, 0),
                };
                let o = Open {
                    kind,
                    id,
                    lon,
                    lat,
                    tags: Vec::new(),
                    refs: Vec::new(),
                    members: Vec::new(),
                };
                if empty {
                    count += 1;
                    finish(o, action, changes)?;
                } else {
                    open = Some(o);
                }
            }
            "tag" => {
                if let Some(o) = open.as_mut() {
                    o.tags.push((req(e, "k")?, req(e, "v")?));
                }
            }
            "nd" => {
                if let Some(o) = open.as_mut() {
                    o.refs.push(req(e, "ref")?.parse().context("bad nd ref")?);
                }
            }
            "member" => {
                if let Some(o) = open.as_mut() {
                    let kind = match req(e, "type")?.as_str() {
                        "node" => Kind::Node,
                        "way" => Kind::Way,
                        "relation" => Kind::Relation,
                        other => bail!("unknown member type '{other}'"),
                    };
                    o.members.push(Member {
                        kind,
                        id: req(e, "ref")?.parse().context("bad member ref")?,
                        role: attr(e, "role")?.unwrap_or_default(),
                    });
                }
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(count)
}

fn finish(o: Open, action: Option<Action>, changes: &mut ChangeSet) -> Result<()> {
    match action {
        Some(Action::Upsert) => changes.upsert(o.finish()),
        Some(Action::Delete) => changes.delete(o.kind, o.id),
        None => bail!("element outside <create>, <modify> or <delete>"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OSC: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<osmChange version="0.6" generator="osmium/1.16.0">
  <delete>
    <node id="1" version="5" lat="49.3537752" lon="8.3982957"/>
    <way id="7" version="2"/>
  </delete>
  <modify>
    <node id="2" version="4" lat="49.9311982" lon="-6.8723385"/>
    <node id="3" version="8" lat="49.8576187" lon="8.5971267">
      <tag k="name" v="Caf&#233; &amp; Bar"/>
    </node>
    <way id="8" version="3">
      <nd ref="2"/>
      <nd ref="3"/>
      <tag k="highway" v="residential"/>
    </way>
  </modify>
  <create>
    <relation id="9" version="1">
      <member type="way" ref="8" role="outer"/>
      <member type="node" ref="3" role=""/>
      <tag k="type" v="multipolygon"/>
    </relation>
  </create>
</osmChange>
"#;

    #[test]
    fn parses_all_actions() {
        let mut cs = ChangeSet::default();
        let n = apply(OSC.as_bytes(), &mut cs).unwrap();
        assert_eq!(n, 6);
        assert_eq!(cs.nodes[&1], None);
        assert_eq!(cs.ways[&7], None);
        assert_eq!(
            cs.nodes[&2],
            Some(Elem::Node {
                id: 2,
                lon: -68723385,
                lat: 499311982,
                tags: vec![]
            })
        );
        let Some(Elem::Node { tags, .. }) = &cs.nodes[&3] else {
            panic!()
        };
        assert_eq!(tags, &vec![("name".to_string(), "Café & Bar".to_string())]);
        let Some(Elem::Way { refs, .. }) = &cs.ways[&8] else {
            panic!()
        };
        assert_eq!(refs, &vec![2, 3]);
        let Some(Elem::Relation { members, .. }) = &cs.relations[&9] else {
            panic!()
        };
        assert_eq!(
            members[0],
            Member {
                kind: Kind::Way,
                id: 8,
                role: "outer".into()
            }
        );
        assert_eq!(members[1].role, "");
    }

    #[test]
    fn later_changes_win() {
        let mut cs = ChangeSet::default();
        apply(OSC.as_bytes(), &mut cs).unwrap();
        let undo = r#"<osmChange><create><node id="1" lat="1" lon="2"/></create><delete><node id="3"/></delete></osmChange>"#;
        apply(undo.as_bytes(), &mut cs).unwrap();
        assert!(cs.nodes[&1].is_some());
        assert_eq!(cs.nodes[&3], None);
    }
}
