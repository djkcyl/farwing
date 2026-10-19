//! Shared Draw payloads; preserve oneof selection and optional-field presence.
#[derive(Clone, PartialEq, prost::Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct PbDrawRequest {
    #[prost(oneof = "PbDrawRequestKind", tags = "1,2,3")]
    pub(crate) payload: Option<PbDrawRequestKind>,
}
#[derive(Clone, PartialEq, prost::Oneof, serde::Serialize, serde::Deserialize)]
pub(crate) enum PbDrawRequestKind {
    #[prost(message, tag = "1")]
    Stroke(PbDrawStroke),
    #[prost(message, tag = "2")]
    Clear(PbDrawClear),
    #[prost(message, tag = "3")]
    Toggle(PbDrawToggle),
}
#[derive(Clone, PartialEq, prost::Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct PbDrawPoint {
    #[prost(float, tag = "1")]
    pub(crate) x: f32,
    #[prost(float, tag = "2")]
    pub(crate) y: f32,
}
#[derive(Clone, PartialEq, prost::Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct PbDrawStroke {
    #[prost(uint32, tag = "1")]
    pub(crate) stroke_id: u32,
    #[prost(message, repeated, tag = "2")]
    pub(crate) points: Vec<PbDrawPoint>,
    #[prost(int32, optional, tag = "3")]
    pub(crate) screen_id: Option<i32>,
    #[prost(float, optional, tag = "4")]
    pub(crate) line_width: Option<f32>,
    #[prost(uint32, optional, tag = "5")]
    pub(crate) color: Option<u32>,
}
#[derive(Clone, PartialEq, prost::Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct PbDrawClear {
    #[prost(int32, tag = "1")]
    pub(crate) clear_type: i32,
    #[prost(uint32, tag = "2")]
    pub(crate) stroke_id: u32,
    #[prost(int32, optional, tag = "3")]
    pub(crate) screen_id: Option<i32>,
}
#[derive(Clone, PartialEq, prost::Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct PbDrawToggle {
    #[prost(bool, tag = "1")]
    pub(crate) enable: bool,
}
