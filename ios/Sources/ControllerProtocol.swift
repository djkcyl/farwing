import Foundation

enum ControllerProtocolError: LocalizedError {
    case invalidText
    case invalidCoordinate

    var errorDescription: String? {
        switch self {
        case .invalidText: "输入内容为空。"
        case .invalidCoordinate: "触控坐标无效。"
        }
    }
}

enum HostControlEvent {
    case echoRequest(sequence: Int64, timestamp: Int64)
    case echoResponse
    case screenCatalog(currentScreenID: Int32, screens: [HostScreenInfo])
    case captureChange(screenID: Int32?)
    case other
}

struct HostScreenInfo: Equatable {
    let id: Int32
    let videoTrackIndex: Int32
    let displayName: String
    let width: Int32?
    let height: Int32?
}

/// The input JSON and the small PB envelope used by Farwing 1.0.2-beta.8.
/// Media negotiation stays in WebRTC; this file deliberately has no SDK imports.
enum ControllerProtocol {
    static let captureSettingLevel: Int64 = 6

    static func absolutePointer(screenID: Int32?, x: Double, y: Double) throws -> Data {
        guard x.isFinite, y.isFinite, (0...1).contains(x), (0...1).contains(y) else {
            throw ControllerProtocolError.invalidCoordinate
        }
        var event: [String: Any] = [
            "action": "mouse_move_absolute",
            "abs_x": x,
            "abs_y": y,
        ]
        if let screenID {
            event["screen_id"] = screenID
        }
        return try jsonData(event)
    }

    static func click() throws -> Data {
        try jsonData(["action": "mouse_click", "button": 1])
    }

    static func text(_ value: String) throws -> Data {
        guard !value.isEmpty else { throw ControllerProtocolError.invalidText }
        return try jsonData(["action": "text_input", "content": value])
    }

    static func key(_ code: UInt16, down: Bool) throws -> Data {
        try jsonData([
            "action": down ? "kbd_press" : "kbd_release",
            "key": Int(code),
        ])
    }

    static func heartbeat() throws -> Data {
        try jsonData(["heartbeat": "kbd_heartbeat"])
    }

    static func echoRequest(sequence: Int64, timestamp: Int64) -> Data {
        echo(action: 0, sequence: sequence, timestamp: timestamp)
    }

    static func echoResponse(to sequence: Int64, timestamp: Int64) -> Data {
        echo(action: 1, sequence: sequence, timestamp: timestamp)
    }

    static func decodeHostControl(_ data: Data) -> HostControlEvent? {
        guard data.first != UInt8(ascii: "{"), let envelope = ProtoReader(data) else { return nil }
        if let payload = envelope.bytes(field: 3) {
            guard let actionData = ProtoReader(payload) else { return nil }
            // Prost omits zero-valued scalar fields, so an absent action is ECHO_REQUEST (0).
            let action = actionData.int32(field: 1) ?? 0
            let sequence = envelope.int64(field: 1) ?? 0
            let timestamp = envelope.int64(field: 2) ?? 0
            if action == 0 {
                return .echoRequest(sequence: sequence, timestamp: timestamp)
            }
            if action == 1 { return .echoResponse }
            return .other
        }
        if let screens = envelope.bytes(field: 7), let source = ProtoReader(screens) {
            let entries = source.repeatedBytes(field: 1).compactMap { raw -> HostScreenInfo? in
                guard let screen = ProtoReader(raw) else { return nil }
                let size = screen.bytes(field: 4).flatMap(ProtoReader.init)
                return HostScreenInfo(
                    id: screen.int32(field: 1) ?? 0,
                    videoTrackIndex: screen.int32(field: 12) ?? -1,
                    displayName: screen.string(field: 10) ?? "",
                    width: size?.int32(field: 3),
                    height: size?.int32(field: 4)
                )
            }
            return .screenCatalog(currentScreenID: source.int32(field: 2) ?? 0, screens: entries)
        }
        if let bytes = envelope.bytes(field: 8), let change = ProtoReader(bytes) {
            // CaptureChange.kind and screen id zero are valid protobuf defaults.
            let kind = change.int32(field: 1) ?? 0
            return .captureChange(screenID: kind == 0 ? (change.int32(field: 2) ?? 0) : nil)
        }
        return .other
    }

    private static func echo(action: Int64, sequence: Int64, timestamp: Int64) -> Data {
        let featureFlags = protobufMessage {
            appendVarintField(1, captureSettingLevel, to: &$0)
        }
        let args = Data("{\"seq\":\(sequence)}".utf8)
        let simpleAction = protobufMessage {
            appendVarintField(1, action, to: &$0)
            appendBytesField(2, args, to: &$0)
            appendBytesField(4, featureFlags, to: &$0)
        }
        return protobufMessage {
            appendVarintField(1, sequence, to: &$0)
            appendVarintField(2, timestamp, to: &$0)
            appendBytesField(3, simpleAction, to: &$0)
        }
    }

    private static func jsonData(_ object: [String: Any]) throws -> Data {
        try JSONSerialization.data(withJSONObject: object, options: [.fragmentsAllowed, .sortedKeys])
    }

    private static func protobufMessage(_ build: (inout Data) -> Void) -> Data {
        var data = Data()
        build(&data)
        return data
    }

    private static func appendVarintField(_ field: Int, _ value: Int64, to data: inout Data) {
        appendVarint(UInt64(field << 3), to: &data)
        appendVarint(UInt64(bitPattern: value), to: &data)
    }

    private static func appendBytesField(_ field: Int, _ value: Data, to data: inout Data) {
        appendVarint(UInt64((field << 3) | 2), to: &data)
        appendVarint(UInt64(value.count), to: &data)
        data.append(value)
    }

    private static func appendVarint(_ value: UInt64, to data: inout Data) {
        var remaining = value
        while remaining >= 0x80 {
            data.append(UInt8(remaining & 0x7f) | 0x80)
            remaining >>= 7
        }
        data.append(UInt8(remaining))
    }
}

private struct ProtoField {
    let number: Int
    let wire: Int
    let varint: UInt64?
    let data: Data?
}

private struct ProtoReader {
    private let fields: [ProtoField]

    init?(_ data: Data) {
        var result: [ProtoField] = []
        var cursor = 0
        while cursor < data.count {
            guard let key = Self.readVarint(data, cursor: &cursor) else { return nil }
            let wire = Int(key & 7)
            let number = Int(key >> 3)
            guard number > 0 else { return nil }
            switch wire {
            case 0:
                guard let value = Self.readVarint(data, cursor: &cursor) else { return nil }
                result.append(ProtoField(number: number, wire: wire, varint: value, data: nil))
            case 1:
                guard cursor <= data.count - 8 else { return nil }
                cursor += 8
            case 2:
                guard let count = Self.readVarint(data, cursor: &cursor),
                      count <= UInt64(data.count - cursor) else { return nil }
                let end = cursor + Int(count)
                result.append(ProtoField(
                    number: number,
                    wire: wire,
                    varint: nil,
                    data: Data(data[cursor..<end])
                ))
                cursor = end
            case 5:
                guard cursor <= data.count - 4 else { return nil }
                cursor += 4
            default:
                return nil
            }
        }
        fields = result
    }

    func int64(field number: Int) -> Int64? {
        guard let value = fields.first(where: { $0.number == number && $0.wire == 0 })?.varint else {
            return nil
        }
        return Int64(bitPattern: value)
    }

    func int32(field number: Int) -> Int32? {
        guard let value = int64(field: number) else { return nil }
        return Int32(truncatingIfNeeded: value)
    }

    func bytes(field number: Int) -> Data? {
        fields.first(where: { $0.number == number && $0.wire == 2 })?.data
    }

    func repeatedBytes(field number: Int) -> [Data] {
        fields.compactMap { $0.number == number && $0.wire == 2 ? $0.data : nil }
    }

    func string(field number: Int) -> String? {
        bytes(field: number).flatMap { String(data: $0, encoding: .utf8) }
    }

    private static func readVarint(_ data: Data, cursor: inout Int) -> UInt64? {
        var value: UInt64 = 0
        for shift in stride(from: 0, through: 63, by: 7) {
            guard cursor < data.count else { return nil }
            let byte = data[cursor]
            cursor += 1
            if shift == 63 && byte > 1 { return nil }
            value |= UInt64(byte & 0x7f) << shift
            if byte & 0x80 == 0 { return value }
        }
        return nil
    }
}
