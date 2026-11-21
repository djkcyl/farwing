import Combine
import Foundation
#if canImport(UIKit)
import UIKit
#endif
import WebRTC

#if canImport(UIKit)
private let defaultControllerName = UIDevice.current.name
#else
private let defaultControllerName = "iPhone / iPad"
#endif

@MainActor
final class ControllerModel: ObservableObject {
    @Published var serverAddress: String
    @Published var controllerName = defaultControllerName
    @Published var pairCode = ""
    @Published private(set) var pairedServers: [PairedServer]
    @Published private(set) var selectedServerOrigin: String?
    @Published private(set) var hosts: [HostDevice] = []
    @Published var selectedHostID: String?
    @Published private(set) var isBusy = false
    @Published private(set) var isStartingSession = false
    @Published private(set) var connectionStatus = "尚未连接"
    @Published private(set) var connectionDetail = "输入 HTTPS 服务端地址以加载设备。"
    @Published private(set) var errorMessage: String?
    @Published private(set) var videoTrack: RTCVideoTrack?
    @Published private(set) var videoSize = CGSize.zero
    @Published private(set) var hasRenderedFrame = false
    @Published private(set) var hostHandshakeReady = false
    @Published var textInput = ""

    private let vault: CredentialVault
    private var activeClient: NativeSessionClient?
    private var activeOrigin: String?

    init(vault: CredentialVault = CredentialVault()) {
        self.vault = vault
        let servers = vault.servers()
        pairedServers = servers
        selectedServerOrigin = servers.first?.origin
        serverAddress = servers.first?.origin ?? ""
    }

    var selectedServer: PairedServer? {
        guard let selectedServerOrigin else { return nil }
        return pairedServers.first { $0.origin == selectedServerOrigin }
    }

    var selectedHost: HostDevice? {
        guard let selectedHostID else { return nil }
        return hosts.first { $0.id == selectedHostID }
    }

    var sessionIsActive: Bool {
        activeClient != nil || isStartingSession
    }

    var canDisconnect: Bool { activeClient != nil }

    func connectServer() async {
        clearError()
        let code = pairCode.trimmingCharacters(in: .whitespacesAndNewlines)
        let givenName = controllerName.trimmingCharacters(in: .whitespacesAndNewlines)
        let name = givenName.isEmpty ? defaultControllerName : givenName
        isBusy = true
        connectionStatus = "正在连接服务器…"
        connectionDetail = "正在验证自建服务器地址。"
        defer { isBusy = false }
        do {
            let origin = try ServerAddress.origin(from: serverAddress)
            let backend = BackendClient(origin: origin)
            let credential: ServerCredential
            if !code.isEmpty {
                let reply = try await backend.pair(code: code, platformName: name)
                credential = ServerCredential(deviceID: reply.deviceId, token: reply.token)
                try vault.setCredential(credential, for: origin)
            } else {
                let saved = try vault.credential(for: origin)
                credential = try saved ?? ServerCredential.generate()
                if saved == nil {
                    try vault.setCredential(credential, for: origin)
                }
                if !pairedServers.contains(where: { $0.origin == origin && $0.deviceID == credential.deviceID }) {
                    try await backend.enroll(with: credential, name: String(name.prefix(80)))
                }
            }
            let server = PairedServer(
                origin: origin,
                deviceID: credential.deviceID,
                name: URL(string: origin)?.host ?? origin,
                pairedAt: Date()
            )
            pairedServers.removeAll { $0.origin == origin }
            pairedServers.insert(server, at: 0)
            vault.save(pairedServers)
            if selectedServerOrigin != origin {
                hosts = []
                selectedHostID = nil
            }
            selectedServerOrigin = origin
            serverAddress = origin
            pairCode = ""
            await refreshHosts()
        } catch {
            showError(error.localizedDescription, status: "服务器连接失败")
        }
    }

    func selectServer(_ origin: String) async {
        guard selectedServerOrigin != origin else { return }
        selectedServerOrigin = origin
        serverAddress = origin
        selectedHostID = nil
        hosts = []
        await refreshHosts()
    }

    func refreshHosts() async {
        guard activeClient == nil else { return }
        clearError()
        guard let server = selectedServer else {
            hosts = []
            connectionStatus = "尚未连接"
            connectionDetail = "输入 HTTPS 服务端地址以加载设备。"
            return
        }
        isBusy = true
        connectionStatus = "正在读取主机…"
        connectionDetail = "正在读取主机列表…"
        defer { isBusy = false }
        do {
            guard let credential = try vault.credential(for: server.origin) else {
                throw BackendError.missingCredential
            }
            hosts = try await BackendClient(origin: server.origin).devices(using: credential)
            if !hosts.contains(where: { $0.id == selectedHostID }) {
                selectedHostID = hosts.first?.id
            }
            connectionStatus = "已连接服务器"
            connectionDetail = hosts.isEmpty ? "在 Windows 被控端保存同一地址，即可在这里找到它。" : "主机列表已更新。"
        } catch {
            showError(error.localizedDescription, status: "读取主机失败")
        }
    }

    func connect(to host: HostDevice) async {
        guard activeClient == nil, !isBusy else { return }
        clearError()
        guard host.online else {
            showError("此主机当前不在线。", status: "无法连接")
            return
        }
        guard host.controllable else {
            showError("此主机当前不允许控制。", status: "无法连接")
            return
        }
        guard let server = selectedServer else {
            showError("请先选择一个已配对的服务端。", status: "无法连接")
            return
        }

        isBusy = true
        isStartingSession = true
        resetMediaState()
        connectionStatus = "正在创建会话…"
        connectionDetail = "正在向 \(server.origin) 请求一次性信令凭据。"
        defer {
            isBusy = false
            isStartingSession = false
        }
        do {
            guard let credential = try vault.credential(for: server.origin) else {
                throw BackendError.missingCredential
            }
            let backend = BackendClient(origin: server.origin)
            let session = try await backend.createSession(for: host.id, using: credential)
            let client = NativeSessionClient(backend: backend, sessionReply: session)
            activeClient = client
            activeOrigin = server.origin
            client.onStatus = { [weak self] status in
                guard let self else { return }
                self.connectionDetail = status
                if status.contains("失败") || status.contains("中断") {
                    self.connectionStatus = "连接异常"
                } else if self.hasRenderedFrame {
                    self.connectionStatus = "已连接"
                } else {
                    self.connectionStatus = status
                }
            }
            client.onVideoTrack = { [weak self] track in
                guard let self else { return }
                let changed = self.videoTrack !== track
                if changed {
                    self.hasRenderedFrame = false
                    self.videoSize = .zero
                    if track != nil { self.connectionStatus = "正在切换视频画面…" }
                }
                self.videoTrack = track
                if track == nil && self.activeClient != nil {
                    self.hasRenderedFrame = false
                    self.videoSize = .zero
                    self.connectionStatus = "主机当前没有活动画面"
                }
            }
            client.onHandshake = { [weak self] in
                guard let self else { return }
                self.hostHandshakeReady = true
                self.connectionDetail = "主机协议握手完成，等待媒体画面…"
            }
            client.onEnded = { [weak self, weak client] reason in
                guard let self else { return }
                if self.activeClient === client {
                    self.activeClient = nil
                    self.activeOrigin = nil
                    self.videoTrack = nil
                    self.hasRenderedFrame = false
                    self.hostHandshakeReady = false
                    self.connectionStatus = "连接已结束"
                    self.connectionDetail = "会话已关闭。"
                    self.errorMessage = reason
                }
            }
            connectionStatus = "正在连接信令…"
            connectionDetail = "已创建会话，正在验证一次性信令凭据。"
            try await client.start()
            if activeClient === client && !hasRenderedFrame {
                connectionStatus = "正在等待视频首帧…"
            }
        } catch {
            let message = error.localizedDescription
            if let client = activeClient, activeOrigin == server.origin {
                await client.stop(sendLeave: true)
            }
            activeClient = nil
            activeOrigin = nil
            resetMediaState()
            showError(message, status: "连接失败")
        }
    }

    func disconnect(fromBackground: Bool = false) async {
        guard let client = activeClient else { return }
        connectionStatus = "正在断开连接…"
        connectionDetail = "正在释放输入并结束远程会话。"
        await client.stop(sendLeave: true)
        if activeClient === client {
            activeClient = nil
            activeOrigin = nil
        }
        resetMediaState()
        connectionStatus = "已断开连接"
        connectionDetail = fromBackground ? "应用进入后台时已结束会话。" : "会话已安全关闭。"
    }

    func didRenderVideoFrame(size: CGSize) {
        guard size.width > 0, size.height > 0 else { return }
        videoSize = size
        if !hasRenderedFrame {
            hasRenderedFrame = true
            connectionStatus = "已连接"
            connectionDetail = "视频画面已开始播放（\(Int(size.width)) × \(Int(size.height))）。"
        }
    }

    func movePointer(x: Double, y: Double) {
        activeClient?.sendPointer(x: x, y: y)
    }

    func click(at point: CGPoint) {
        activeClient?.click(at: point.x, y: point.y)
    }

    func submitText() {
        guard !textInput.isEmpty else { return }
        if activeClient?.sendText(textInput) == true {
            textInput = ""
        } else {
            errorMessage = "控制通道尚未就绪，文字没有发送。"
        }
    }

    func tapKey(_ code: UInt16) {
        activeClient?.tapKey(code)
    }

    func forgetServer(_ origin: String) async {
        if activeOrigin == origin { await disconnect() }
        do {
            try vault.forget(origin: origin)
            pairedServers.removeAll { $0.origin == origin }
            if selectedServerOrigin == origin {
                selectedServerOrigin = pairedServers.first?.origin
                serverAddress = selectedServerOrigin ?? ""
                hosts = []
                selectedHostID = nil
                await refreshHosts()
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func clearError() {
        errorMessage = nil
    }

    private func resetMediaState() {
        videoTrack = nil
        videoSize = .zero
        hasRenderedFrame = false
        hostHandshakeReady = false
    }

    private func showError(_ message: String, status: String) {
        errorMessage = message
        connectionStatus = status
        connectionDetail = message
    }
}
