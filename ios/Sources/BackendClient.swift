import CryptoKit
import Foundation
import Security

struct ServerCredential: Codable, Equatable {
    let deviceID: String
    let token: String

    static func generate() throws -> ServerCredential {
        var random = [UInt8](repeating: 0, count: 32)
        let status = random.withUnsafeMutableBytes { bytes in
            SecRandomCopyBytes(kSecRandomDefault, bytes.count, bytes.baseAddress!)
        }
        guard status == errSecSuccess else { throw BackendError.keychain(status) }
        let token = random.map { String(format: "%02x", $0) }.joined()
        let digest = SHA256.hash(data: Data(token.utf8))
        return ServerCredential(deviceID: digest.prefix(8).map { String(format: "%02x", $0) }.joined(), token: token)
    }
}

struct PairedServer: Codable, Identifiable, Equatable {
    let origin: String
    let deviceID: String
    var name: String
    var pairedAt: Date

    var id: String { origin }
}

struct HostDevice: Decodable, Identifiable, Equatable {
    let id: String
    let name: String
    let online: Bool
    let controllable: Bool
}

struct PairReply: Decodable {
    let deviceId: String
    let token: String
}

struct CreateSessionReply: Decodable {
    let sessionId: String
    let ticket: String
    let signalingUrl: URL
    let iceServers: [IceServerReply]
}

struct IceServerReply: Decodable {
    let urls: [String]
    let username: String?
    let credential: String?

    private enum CodingKeys: String, CodingKey {
        case urls
        case username
        case credential
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        if let many = try? container.decode([String].self, forKey: .urls) {
            urls = many
        } else {
            urls = [try container.decode(String.self, forKey: .urls)]
        }
        username = try container.decodeIfPresent(String.self, forKey: .username)
        credential = try container.decodeIfPresent(String.self, forKey: .credential)
    }
}

enum BackendError: LocalizedError {
    case invalidServerAddress
    case invalidSignalingAddress
    case invalidResponse
    case httpStatus(Int, String)
    case missingCredential
    case keychain(OSStatus)

    var errorDescription: String? {
        switch self {
        case .invalidServerAddress:
            "服务端地址必须是 HTTPS origin，例如 https://192.168.1.10:8443。"
        case .invalidSignalingAddress:
            "服务端返回了非同源或非安全的信令地址，已拒绝连接。"
        case .invalidResponse:
            "服务端返回的数据格式不正确。"
        case let .httpStatus(code, message):
            message.isEmpty ? "服务端请求失败（HTTP \(code)）。" : message
        case .missingCredential:
            "此服务端没有可用的配对令牌，请重新配对。"
        case let .keychain(status):
            "无法安全保存配对令牌（Keychain \(status)）。"
        }
    }
}

enum ServerAddress {
    static func origin(from text: String) throws -> String {
        let raw = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let components = URLComponents(string: raw),
              components.scheme?.lowercased() == "https",
              let host = components.host, !host.isEmpty,
              components.user == nil, components.password == nil,
              components.query == nil, components.fragment == nil,
              components.path.isEmpty || components.path == "/" else {
            throw BackendError.invalidServerAddress
        }
        return try canonicalOrigin(components, forceSecure: true)
    }

    static func signalingURL(_ textURL: URL, for origin: String) throws -> URL {
        guard var components = URLComponents(url: textURL, resolvingAgainstBaseURL: false),
              components.scheme?.lowercased() == "wss",
              components.user == nil, components.password == nil,
              components.path == "/v1/session",
              components.query == nil, components.fragment == nil,
              try canonicalOrigin(components, forceSecure: true) == origin else {
            throw BackendError.invalidSignalingAddress
        }
        components.scheme = "wss"
        guard let url = components.url else { throw BackendError.invalidSignalingAddress }
        return url
    }

    static func httpsOrigin(of url: URL) throws -> String {
        guard let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
              components.scheme?.lowercased() == "https",
              components.user == nil, components.password == nil else {
            throw BackendError.invalidServerAddress
        }
        return try canonicalOrigin(components, forceSecure: true)
    }

    private static func canonicalOrigin(_ input: URLComponents, forceSecure: Bool) throws -> String {
        var components = input
        components.scheme = forceSecure ? "https" : components.scheme?.lowercased()
        components.host = components.host?.lowercased()
        if components.port == 443 { components.port = nil }
        components.path = ""
        components.query = nil
        components.fragment = nil
        guard let result = components.string else { throw BackendError.invalidServerAddress }
        return result
    }
}

/// Bearer credentials stay in Keychain and are never added to a URL or logs.
final class CredentialVault {
    private let service = "org.openuuyc.controller.ios.credentials.v1"
    private let defaultsKey = "org.openuuyc.controller.ios.pairedServers.v1"
    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    func servers() -> [PairedServer] {
        guard let data = defaults.data(forKey: defaultsKey),
              let value = try? JSONDecoder().decode([PairedServer].self, from: data) else {
            return []
        }
        return value.sorted { $0.pairedAt > $1.pairedAt }
    }

    func save(_ servers: [PairedServer]) {
        guard let data = try? JSONEncoder().encode(servers) else { return }
        defaults.set(data, forKey: defaultsKey)
    }

    func credential(for origin: String) throws -> ServerCredential? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: origin,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else {
            throw BackendError.keychain(status)
        }
        do {
            return try JSONDecoder().decode(ServerCredential.self, from: data)
        } catch {
            throw BackendError.invalidResponse
        }
    }

    func setCredential(_ credential: ServerCredential, for origin: String) throws {
        let data = try JSONEncoder().encode(credential)
        let identity: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: origin,
        ]
        let update: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        ]
        let status = SecItemUpdate(identity as CFDictionary, update as CFDictionary)
        if status == errSecItemNotFound {
            var insert = identity
            update.forEach { insert[$0.key] = $0.value }
            let added = SecItemAdd(insert as CFDictionary, nil)
            guard added == errSecSuccess else { throw BackendError.keychain(added) }
        } else if status != errSecSuccess {
            throw BackendError.keychain(status)
        }
    }

    func forget(origin: String) throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: origin,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw BackendError.keychain(status)
        }
        save(servers().filter { $0.origin != origin })
    }
}

private final class SameOriginRedirectDelegate: NSObject, URLSessionTaskDelegate {
    private let origin: String

    init(origin: String) {
        self.origin = origin
    }

    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        guard let target = request.url,
              (try? ServerAddress.httpsOrigin(of: target)) == origin else {
            completionHandler(nil)
            return
        }
        completionHandler(request)
    }
}

final class BackendClient: NSObject {
    let origin: String
    private var session: URLSession!
    private var webSocketSession: URLSession!
    private let redirectDelegate: SameOriginRedirectDelegate

    init(origin: String) {
        self.origin = origin
        let configuration = URLSessionConfiguration.ephemeral
        configuration.urlCache = nil
        configuration.httpCookieStorage = nil
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.timeoutIntervalForRequest = 20
        configuration.timeoutIntervalForResource = 45
        redirectDelegate = SameOriginRedirectDelegate(origin: origin)
        super.init()
        session = URLSession(configuration: configuration, delegate: redirectDelegate, delegateQueue: nil)
        let socketConfiguration = URLSessionConfiguration.ephemeral
        socketConfiguration.urlCache = nil
        socketConfiguration.httpCookieStorage = nil
        socketConfiguration.timeoutIntervalForRequest = 60
        socketConfiguration.timeoutIntervalForResource = 24 * 60 * 60
        webSocketSession = URLSession(
            configuration: socketConfiguration,
            delegate: redirectDelegate,
            delegateQueue: nil
        )
    }

    func enroll(with credential: ServerCredential, name: String) async throws {
        struct Reply: Decodable { let device_id: String }
        let reply = try await request(path: "/v1/desktop/register", method: "POST", token: credential.token,
                                      body: ["platform": "ios", "name": name], responseType: Reply.self)
        guard reply.device_id == credential.deviceID else { throw BackendError.invalidResponse }
    }

    func pair(code: String, platformName: String) async throws -> PairReply {
        try await request(
            path: "/v1/pair",
            method: "POST",
            body: ["role": "controller", "platform": "ios", "name": platformName, "code": code],
            responseType: PairReply.self
        )
    }

    func devices(using credential: ServerCredential) async throws -> [HostDevice] {
        struct Reply: Decodable { let devices: [HostDevice] }
        return try await request(
            path: "/v1/devices",
            method: "GET",
            token: credential.token,
            responseType: Reply.self
        ).devices
    }

    func createSession(for deviceID: String, using credential: ServerCredential) async throws -> CreateSessionReply {
        let reply: CreateSessionReply = try await request(
            path: "/v1/sessions",
            method: "POST",
            token: credential.token,
            body: ["deviceId": deviceID],
            responseType: CreateSessionReply.self
        )
        _ = try ServerAddress.signalingURL(reply.signalingUrl, for: origin)
        return reply
    }

    func webSocketTask(for textURL: URL) throws -> URLSessionWebSocketTask {
        let url = try ServerAddress.signalingURL(textURL, for: origin)
        return webSocketSession.webSocketTask(with: url)
    }

    private func request<Response: Decodable>(
        path: String,
        method: String,
        token: String? = nil,
        body: [String: String]? = nil,
        responseType: Response.Type
    ) async throws -> Response {
        guard let url = URL(string: origin + path) else { throw BackendError.invalidServerAddress }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONSerialization.data(withJSONObject: body, options: [.sortedKeys])
        }
        if let token {
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }
        let (data, response) = try await session.data(for: request)
        guard let http = response as? HTTPURLResponse else { throw BackendError.invalidResponse }
        guard (200..<300).contains(http.statusCode) else {
            throw BackendError.httpStatus(http.statusCode, Self.message(from: data))
        }
        do {
            return try JSONDecoder().decode(responseType, from: data)
        } catch {
            throw BackendError.invalidResponse
        }
    }

    private static func message(from data: Data) -> String {
        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return "" }
        for key in ["message", "error"] {
            if let message = object[key] as? String, !message.isEmpty {
                return String(message.prefix(300))
            }
        }
        return ""
    }

}
