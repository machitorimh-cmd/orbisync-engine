//! Example application policy: move a logical position by at most one unit.
use orbisync_server::input::{InputContext, InputRule};
use prost_types::{Struct, Value, value::Kind};

pub struct Movement;
impl InputRule for Movement {
    fn component_key(&self) -> &str {
        "example.position"
    }
    fn compute(&self, context: InputContext<'_>, intent: &Struct) -> Result<Struct, String> {
        let dx = match intent
            .fields
            .get("dx")
            .and_then(|value| value.kind.as_ref())
        {
            Some(Kind::NumberValue(dx)) if dx.is_finite() && dx.abs() <= 1.0 => *dx,
            _ => return Err("dx must be a finite step between -1 and 1".into()),
        };
        let x = match context.entity.components().get("example.position") {
            Some(bytes) => serde_json::from_slice::<serde_json::Value>(bytes)
                .ok()
                .and_then(|value| value.get("x").and_then(|x| x.as_f64()))
                .ok_or("invalid canonical position")?,
            None => 0.0,
        };
        Ok(Struct {
            fields: [
                (
                    "component_key".into(),
                    Value {
                        kind: Some(Kind::StringValue("example.position".into())),
                    },
                ),
                (
                    "x".into(),
                    Value {
                        kind: Some(Kind::NumberValue(x + dx)),
                    },
                ),
            ]
            .into(),
        })
    }
}
