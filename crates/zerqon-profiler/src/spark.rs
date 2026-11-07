use std::collections::BTreeMap;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SamplerData {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(message, repeated, tag = "2")]
    pub threads: Vec<Thread>,
    #[prost(int32, repeated, tag = "6")]
    pub time_windows: Vec<i32>,
    #[prost(btree_map = "int32, message", tag = "7")]
    pub windows: BTreeMap<i32, Window>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Metadata {
    #[prost(int64, tag = "2")]
    pub start_time: i64,
    #[prost(int32, tag = "3")]
    pub interval: i32,
    #[prost(message, optional, tag = "4")]
    pub thread_dumper: Option<ThreadDumper>,
    #[prost(message, optional, tag = "5")]
    pub aggregator: Option<Aggregator>,
    #[prost(string, tag = "6")]
    pub comment: String,
    #[prost(message, optional, tag = "7")]
    pub platform: Option<Platform>,
    #[prost(int64, tag = "11")]
    pub end_time: i64,
    #[prost(btree_map = "string, string", tag = "14")]
    pub extra: BTreeMap<String, String>,
    #[prost(int32, tag = "15")]
    pub mode: i32,
    #[prost(int32, tag = "16")]
    pub engine: i32,
    #[prost(string, tag = "17")]
    pub engine_version: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Platform {
    #[prost(int32, tag = "1")]
    pub kind: i32,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub version: String,
    #[prost(string, tag = "4")]
    pub minecraft_version: String,
    #[prost(int32, tag = "7")]
    pub data_version: i32,
    #[prost(string, tag = "8")]
    pub brand: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ThreadDumper {
    #[prost(int32, tag = "1")]
    pub kind: i32,
    #[prost(int64, repeated, tag = "2")]
    pub ids: Vec<i64>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Aggregator {
    #[prost(int32, tag = "1")]
    pub kind: i32,
    #[prost(int32, tag = "2")]
    pub thread_grouper: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Thread {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, repeated, tag = "3")]
    pub children: Vec<Node>,
    #[prost(double, repeated, tag = "4")]
    pub times: Vec<f64>,
    #[prost(int32, repeated, tag = "5")]
    pub children_refs: Vec<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Node {
    #[prost(string, tag = "3")]
    pub class_name: String,
    #[prost(string, tag = "4")]
    pub method_name: String,
    #[prost(int32, tag = "5")]
    pub parent_line: i32,
    #[prost(int32, tag = "6")]
    pub line: i32,
    #[prost(double, repeated, tag = "8")]
    pub times: Vec<f64>,
    #[prost(int32, repeated, tag = "9")]
    pub children_refs: Vec<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Window {
    #[prost(int64, tag = "11")]
    pub start_time: i64,
    #[prost(int64, tag = "12")]
    pub end_time: i64,
    #[prost(int32, tag = "13")]
    pub duration: i32,
}
