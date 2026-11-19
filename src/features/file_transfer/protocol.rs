//! Official 4.40.1 file-transfer wire contract.
use prost::{Message, Oneof};
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileRename {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub new_name: String,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileDirCreate {
    #[prost(string, tag = "1")]
    pub path: String,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileRemoveFile {
    #[prost(string, tag = "1")]
    pub path: String,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileOperationResult {
    #[prost(int32, tag = "1")]
    pub file_error: i32,
    #[prost(string, tag = "2")]
    pub err_msg: String,
    #[prost(string, tag = "3")]
    pub path: String,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileExist {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, repeated, tag = "2")]
    pub names: Vec<String>,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileExistResult {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(bool, tag = "2")]
    pub has_same: bool,
    #[prost(bool, tag = "3")]
    pub has_transfering: bool,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileExistResponse {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(message, repeated, tag = "2")]
    pub results: Vec<FileExistResult>,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileEntry {
    #[prost(int32, tag = "1")]
    pub entry_type: i32,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(uint64, tag = "3")]
    pub size: u64,
    #[prost(uint64, tag = "4")]
    pub modified_time: u64,
    #[prost(string, tag = "5")]
    pub full_path: String,
    #[prost(string, tag = "6")]
    pub icon_type: String,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct TaskId {
    #[prost(int32, tag = "1")]
    pub task_id: i32,
    #[prost(int32, tag = "2")]
    pub file_index: i32,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct ReadDir {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(string, tag = "2")]
    pub path: String,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct DirectoryData {
    #[prost(message, repeated, tag = "1")]
    pub entries: Vec<FileEntry>,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileDirectory {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(string, tag = "2")]
    pub path: String,
    #[prost(message, repeated, tag = "3")]
    pub entries: Vec<FileEntry>,
    #[prost(int32, tag = "4")]
    pub file_error: i32,
    #[prost(bytes, tag = "5")]
    pub dir_data: Vec<u8>,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileRemoveDir {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(string, tag = "2")]
    pub path: String,
    #[prost(bool, tag = "3")]
    pub recursive: bool,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferResult {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(int32, tag = "2")]
    pub file_error: i32,
    #[prost(string, tag = "3")]
    pub err_msg: String,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferComplete {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(int32, tag = "2")]
    pub error: i32,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferAsk {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(uint64, tag = "2")]
    pub last_modified: u64,
    #[prost(uint64, tag = "3")]
    pub file_size: u64,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferConfirm {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(bool, tag = "2")]
    pub skip: bool,
    #[prost(int32, tag = "3")]
    pub err: i32,
    #[prost(uint64, tag = "4")]
    pub resume_point: u64,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferBlock {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(int32, tag = "2")]
    pub block_id: i32,
    #[prost(bytes, tag = "3")]
    pub data: Vec<u8>,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferBlockConfirm {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(int32, tag = "2")]
    pub block_id: i32,
    #[prost(int32, tag = "3")]
    pub err: i32,
    #[prost(int32, tag = "4")]
    pub block_len: i32,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileInfo {
    #[prost(string, tag = "1")]
    pub rel_path: String,
    #[prost(uint64, tag = "2")]
    pub size: u64,
    #[prost(uint64, tag = "3")]
    pub modified_time: u64,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileList {
    #[prost(message, repeated, tag = "1")]
    pub files: Vec<FileInfo>,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct DirectorySizeRead {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(uint64, tag = "2")]
    pub size: u64,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferSendRequest {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(string, tag = "2")]
    pub path: String,
    #[prost(bytes, tag = "3")]
    pub list_data: Vec<u8>,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferSendResponse {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(int32, tag = "2")]
    pub err: i32,
    #[prost(message, repeated, tag = "3")]
    pub files: Vec<FileInfo>,
    #[prost(string, tag = "4")]
    pub folder_name: String,
    #[prost(bytes, tag = "5")]
    pub list_data: Vec<u8>,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClearSendTemp {
    #[prost(string, tag = "1")]
    pub task_unique_id: String,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferReceiveRequest {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(string, tag = "2")]
    pub path: String,
    #[prost(message, repeated, tag = "3")]
    pub files: Vec<FileInfo>,
    #[prost(int32, tag = "4")]
    pub file_op_strategy: i32,
    #[prost(string, tag = "5")]
    pub folder_name: String,
    #[prost(bytes, tag = "6")]
    pub list_data: Vec<u8>,
    #[prost(string, tag = "7")]
    pub task_unique_id: String,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferReceiveResponse {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TaskId>,
    #[prost(message, repeated, tag = "2")]
    pub files: Vec<FileInfo>,
    #[prost(int32, tag = "3")]
    pub err: i32,
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferFtpRequest {
    #[prost(
        oneof = "FileTransferFtpRequestKind",
        tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13"
    )]
    pub which: Option<FileTransferFtpRequestKind>,
}
#[derive(Clone, PartialEq, Oneof, serde::Serialize, serde::Deserialize)]
pub(crate) enum FileTransferFtpRequestKind {
    #[prost(message, tag = "1")]
    ReceiveRequest(FileTransferReceiveRequest),
    #[prost(message, tag = "2")]
    SendRequest(FileTransferSendRequest),
    #[prost(message, tag = "3")]
    FileAsk(FileTransferAsk),
    #[prost(message, tag = "4")]
    FileBlock(FileTransferBlock),
    #[prost(message, tag = "5")]
    Rename(FileRename),
    #[prost(message, tag = "6")]
    DirCreate(FileDirCreate),
    #[prost(message, tag = "7")]
    RemoveFile(FileRemoveFile),
    #[prost(message, tag = "8")]
    RemoveDir(FileRemoveDir),
    #[prost(message, tag = "9")]
    Complete(FileTransferComplete),
    #[prost(message, tag = "10")]
    ReadDir(ReadDir),
    #[prost(message, tag = "11")]
    FileExist(FileExist),
    #[prost(message, tag = "12")]
    ClearSendTemp(ClearSendTemp),
    #[prost(message, tag = "13")]
    DirSizeRead(DirectorySizeRead),
}
#[derive(Clone, PartialEq, Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileTransferFtpResponse {
    #[prost(oneof = "FileTransferFtpResponseKind", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub which: Option<FileTransferFtpResponseKind>,
}
#[derive(Clone, PartialEq, Oneof, serde::Serialize, serde::Deserialize)]
pub(crate) enum FileTransferFtpResponseKind {
    #[prost(message, tag = "1")]
    ReceiveResponse(FileTransferReceiveResponse),
    #[prost(message, tag = "2")]
    SendResponse(FileTransferSendResponse),
    #[prost(message, tag = "3")]
    FileConfirm(FileTransferConfirm),
    #[prost(message, tag = "4")]
    BlockConfirm(FileTransferBlockConfirm),
    #[prost(message, tag = "5")]
    OperationResult(FileOperationResult),
    #[prost(message, tag = "6")]
    Result(FileTransferResult),
    #[prost(message, tag = "7")]
    FileDirectory(FileDirectory),
    #[prost(message, tag = "8")]
    FileExistResponse(FileExistResponse),
}
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Envelope {
    #[prost(
        oneof = "EnvelopeKind",
        tags = "3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28"
    )]
    pub which: Option<EnvelopeKind>,
}
#[derive(Clone, PartialEq, Oneof)]
pub(crate) enum EnvelopeKind {
    #[prost(bytes = "bytes", tag = "3")]
    Other3(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "4")]
    Other4(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "5")]
    Other5(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "6")]
    Other6(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "7")]
    Other7(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "8")]
    Other8(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "9")]
    Other9(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "10")]
    Other10(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "11")]
    Other11(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "12")]
    Other12(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "13")]
    Other13(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "14")]
    Other14(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "15")]
    Other15(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "16")]
    Other16(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "17")]
    Other17(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "18")]
    Other18(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "19")]
    Other19(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "20")]
    Other20(bytes::Bytes),
    #[prost(message, tag = "21")]
    Request(Request),
    #[prost(message, tag = "22")]
    Response(Response),
    #[prost(bytes = "bytes", tag = "23")]
    Other23(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "24")]
    Other24(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "25")]
    Other25(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "26")]
    Other26(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "27")]
    Other27(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "28")]
    Other28(bytes::Bytes),
}
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Request {
    #[prost(message, optional, tag = "1")]
    pub header: Option<Header>,
    #[prost(
        oneof = "RequestKind",
        tags = "2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30"
    )]
    pub which: Option<RequestKind>,
}
#[derive(Clone, PartialEq, Oneof)]
pub(crate) enum RequestKind {
    #[prost(bytes = "bytes", tag = "2")]
    Other2(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "3")]
    Other3(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "4")]
    Other4(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "5")]
    Other5(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "6")]
    Other6(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "7")]
    Other7(bytes::Bytes),
    #[prost(message, tag = "8")]
    File(FileTransferFtpRequest),
    #[prost(bytes = "bytes", tag = "9")]
    Other9(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "10")]
    Other10(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "11")]
    Other11(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "12")]
    Other12(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "13")]
    Other13(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "14")]
    Other14(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "15")]
    Other15(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "16")]
    Other16(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "17")]
    Other17(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "18")]
    Other18(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "19")]
    Other19(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "20")]
    Other20(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "21")]
    Other21(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "22")]
    Other22(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "23")]
    Other23(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "24")]
    Other24(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "25")]
    Other25(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "26")]
    Other26(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "27")]
    Other27(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "28")]
    Other28(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "29")]
    Other29(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "30")]
    Other30(bytes::Bytes),
}
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Response {
    #[prost(message, optional, tag = "1")]
    pub header: Option<Header>,
    #[prost(
        oneof = "ResponseKind",
        tags = "2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 26, 27"
    )]
    pub which: Option<ResponseKind>,
}
#[derive(Clone, PartialEq, Oneof)]
pub(crate) enum ResponseKind {
    #[prost(bytes = "bytes", tag = "2")]
    Other2(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "3")]
    Other3(bytes::Bytes),
    #[prost(message, tag = "4")]
    File(FileTransferFtpResponse),
    #[prost(bytes = "bytes", tag = "5")]
    Other5(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "6")]
    Other6(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "7")]
    Other7(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "8")]
    Other8(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "9")]
    Other9(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "10")]
    Other10(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "11")]
    Other11(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "12")]
    Other12(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "13")]
    Other13(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "14")]
    Other14(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "15")]
    Other15(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "16")]
    Other16(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "17")]
    Other17(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "18")]
    Other18(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "19")]
    Other19(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "20")]
    Other20(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "21")]
    Other21(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "22")]
    Other22(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "23")]
    Other23(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "24")]
    Other24(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "26")]
    Other26(bytes::Bytes),
    #[prost(bytes = "bytes", tag = "27")]
    Other27(bytes::Bytes),
}
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Header {
    #[prost(int64, tag = "1")]
    pub id: i64,
}
pub(crate) use FileTransferFtpRequestKind as Req;
pub(crate) use FileTransferFtpResponseKind as Res;
