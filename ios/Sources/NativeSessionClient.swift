import Foundation
import WebRTC

private enum NativeSessionError: LocalizedError {
    case signalingTimeout
    case answerTimeout
    case invalidSignalingMessage
    case peerConnectionUnavailable
    case missingMediaAnswer
    case missingCodec
    case inputUnavailable

    var errorDescription: String? {
        switch self {
        case .signalingTimeout: "信令服务没有及时返回 ready。"
        case .answerTimeout: "信令服务没有及时返回媒体应答。"
        case .invalidSignalingMessage: "收到无法识别的信令数据。"
        case .peerConnectionUnavailable: "无法创建 WebRTC 媒体连接。"
        case .missingMediaAnswer: "媒体应答缺少 SDP。"
        case .missingCodec: "WebRTC SDK 未提供 H.264 视频解码或 Opus 音频解码。"
        case .inputUnavailable: "控制通道尚未就绪。"
        }
    }
}

/// One disposable signaling/WebRTC lifetime. The API bearer token never enters this class;
/// the one-use session ticket is sent only in the first authenticated WS message.
final class NativeSessionClient: NSObject, RTCPeerConnectionDelegate, RTCDataChannelDelegate {
    var onStatus: ((String) -> Void)?
    var onVideoTrack: ((RTCVideoTrack?) -> Void)?
    var onHandshake: (() -> Void)?
    var onEnded: ((String) -> Void)?

    private let backend: BackendClient
    private let sessionReply: CreateSessionReply
    private let origin: String
    private let lock = NSRecursiveLock()
    private let signalSendQueue = DispatchQueue(label: "org.openuuyc.controller.ios.signaling-send")

    private var socket: URLSessionWebSocketTask?
    private var readerTask: Task<Void, Never>?
    private var readyTimeoutTask: Task<Void, Never>?
    private var answerTimeoutTask: Task<Void, Never>?
    private var readyResult: Result<Void, Error>?
    private var readyContinuation: CheckedContinuation<Void, Error>?
    private var ready = false
    private var offerSent = false
    private var localCandidates: [RTCIceCandidate] = []
    private var remoteDescriptionSet = false
    private var remoteCandidates: [RTCIceCandidate] = []
    private var isStopping = false
    private var terminalEventSent = false
    private var nextSequence: Int64 = 1

    private var peerConnection: RTCPeerConnection?
    private var controlChannel: RTCDataChannel?
    private var textChannel: RTCDataChannel?
    private var sentControllerEcho = false
    private var receivedHandshakeResponse = false
    private var heldKeys = Set<UInt16>()
    private var lastPointerSendTime: TimeInterval = 0

    private var videoTracks: [Int: RTCVideoTrack] = [:]
    private var nextVideoIndex = 0
    private var hostScreens: [Int32: HostScreenInfo] = [:]
    private var selectedScreenID: Int32?
    private var captureStateKnown = false

    init(backend: BackendClient, sessionReply: CreateSessionReply) {
        self.backend = backend
        self.sessionReply = sessionReply
        origin = backend.origin
        super.init()
    }

    func start() async throws {
        let socket = try backend.webSocketTask(for: sessionReply.signalingUrl)
        lock.withLock { self.socket = socket }
        socket.resume()

        let authenticate = try JSONSerialization.data(
            withJSONObject: ["type": "authenticate", "ticket": sessionReply.ticket],
            options: [.sortedKeys]
        )
        try await socket.send(.string(String(decoding: authenticate, as: UTF8.self)))

        readerTask = Task { [weak self] in
            guard let self else { return }
            await self.readSignaling()
        }
        try await waitUntilReady()
        publishStatus("信令已就绪，正在协商媒体…")
        try createPeerConnection()
        answerTimeoutTask = Task { [weak self] in
            do {
                try await Task.sleep(nanoseconds: 30_000_000_000)
                guard let self else { return }
                let stillWaiting = self.lock.withLock { !self.remoteDescriptionSet && !self.isStopping }
                if stillWaiting { self.fail(NativeSessionError.answerTimeout.localizedDescription) }
            } catch {
                // The answer arrived or the session ended before the timeout.
            }
        }
        try await makeAndSendOffer()
    }

    func sendPointer(x: Double, y: Double) {
        sendPointer(x: x, y: y, force: false)
    }

    func click(at x: Double, y: Double) {
        sendPointer(x: x, y: y, force: true)
        click()
    }

    private func sendPointer(x: Double, y: Double, force: Bool) {
        let now = ProcessInfo.processInfo.systemUptime
        lock.lock()
        guard !isStopping, force || now - lastPointerSendTime >= 1.0 / 30.0 else {
            lock.unlock()
            return
        }
        lastPointerSendTime = now
        let screenID = selectedScreenID
        lock.unlock()
        guard let data = try? ControllerProtocol.absolutePointer(screenID: screenID, x: x, y: y) else {
            return
        }
        _ = sendInput(data)
    }

    func click() {
        guard let data = try? ControllerProtocol.click() else { return }
        _ = sendInput(data)
    }

    func sendText(_ text: String) -> Bool {
        guard let data = try? ControllerProtocol.text(text) else { return false }
        return sendInput(data)
    }

    func tapKey(_ code: UInt16) {
        guard let channel = readyControlChannel() else { return }
        lock.lock()
        heldKeys.insert(code)
        lock.unlock()
        guard let down = try? ControllerProtocol.key(code, down: true),
              sendInput(down, channel: channel) else {
            lock.lock()
            heldKeys.remove(code)
            lock.unlock()
            return
        }
        if let up = try? ControllerProtocol.key(code, down: false), sendInput(up, channel: channel) {
            lock.lock()
            heldKeys.remove(code)
            lock.unlock()
        }
    }

    func stop(sendLeave: Bool) async {
        let snapshot = beginStopping()
        guard snapshot.shouldStop else { return }
        releaseKeys(snapshot.keys, on: snapshot.controlChannel)
        if sendLeave {
            if let data = try? JSONSerialization.data(withJSONObject: ["type": "leave"]),
               let socket = snapshot.socket {
                socket.send(.string(String(decoding: data, as: UTF8.self))) { _ in }
            }
        }
        readyTimeoutTask?.cancel()
        answerTimeoutTask?.cancel()
        readerTask?.cancel()
        if let socket = snapshot.socket {
            socket.cancel(with: .goingAway, reason: nil)
        }
        snapshot.controlChannel?.close()
        snapshot.textChannel?.close()
        snapshot.peerConnection?.close()
        lock.withLock {
            socket = nil
            controlChannel = nil
            textChannel = nil
            peerConnection = nil
            videoTracks.removeAll()
        }
        publishVideoTrack(nil)
    }

    private func createPeerConnection() throws {
        let configuration = RTCConfiguration()
        configuration.sdpSemantics = .unifiedPlan
        configuration.bundlePolicy = .maxBundle
        configuration.iceServers = sessionReply.iceServers.map {
            RTCIceServer(urlStrings: $0.urls, username: $0.username, credential: $0.credential)
        }
        let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
        guard let peer = Self.factory.peerConnection(
            with: configuration,
            constraints: constraints,
            delegate: self
        ) else {
            throw NativeSessionError.peerConnectionUnavailable
        }

        let channelConfiguration = RTCDataChannelConfiguration()
        channelConfiguration.isOrdered = true
        channelConfiguration.maxPacketLifeTime = -1
        channelConfiguration.maxRetransmits = -1
        guard let control = peer.dataChannel(
            forLabel: "CONTROL_DATA_CHANNEL",
            configuration: channelConfiguration
        ) else {
            throw NativeSessionError.peerConnectionUnavailable
        }
        guard let text = peer.dataChannel(
            forLabel: "TEXT_DATA_CHANNEL",
            configuration: channelConfiguration
        ) else {
            control.close()
            throw NativeSessionError.peerConnectionUnavailable
        }
        control.delegate = self
        text.delegate = self

        let audio = RTCRtpTransceiverInit()
        audio.direction = .recvOnly
        let video = RTCRtpTransceiverInit()
        video.direction = .recvOnly
        guard let audioTransceiver = peer.addTransceiver(of: .audio, init: audio),
              let videoTransceiver = peer.addTransceiver(of: .video, init: video) else {
            control.close()
            text.close()
            throw NativeSessionError.peerConnectionUnavailable
        }

        let audioCodecs = Self.factory.rtpReceiverCapabilities(forKind: "audio").codecs.filter {
            $0.mimeType.caseInsensitiveCompare("audio/opus") == .orderedSame
        }
        let availableVideoCodecs = Self.factory.rtpReceiverCapabilities(forKind: "video").codecs
        let h264Codecs = availableVideoCodecs.filter {
            $0.mimeType.caseInsensitiveCompare("video/h264") == .orderedSame
        }
        let h264PayloadTypes = Set(h264Codecs.compactMap { $0.preferredPayloadType?.stringValue })
        let h264RtxCodecs = availableVideoCodecs.filter { codec in
            codec.mimeType.caseInsensitiveCompare("video/rtx") == .orderedSame
                && codec.parameters["apt"].map(h264PayloadTypes.contains) == true
        }
        let videoCodecs = h264Codecs + h264RtxCodecs
        guard !audioCodecs.isEmpty, !h264Codecs.isEmpty else {
            control.close()
            text.close()
            throw NativeSessionError.missingCodec
        }
        do {
            try audioTransceiver.setCodecPreferences(audioCodecs, error: ())
            try videoTransceiver.setCodecPreferences(videoCodecs, error: ())
        } catch {
            control.close()
            text.close()
            throw NativeSessionError.missingCodec
        }

        lock.lock()
        peerConnection = peer
        controlChannel = control
        textChannel = text
        lock.unlock()
    }

    private func makeAndSendOffer() async throws {
        guard let peer = currentPeerConnection() else {
            throw NativeSessionError.peerConnectionUnavailable
        }
        let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
        let offer: RTCSessionDescription = try await withCheckedThrowingContinuation {
            (continuation: CheckedContinuation<RTCSessionDescription, Error>) in
            peer.offer(for: constraints) { description, error in
                if let error {
                    continuation.resume(throwing: error)
                } else if let description {
                    continuation.resume(returning: description)
                } else {
                    continuation.resume(throwing: NativeSessionError.missingMediaAnswer)
                }
            }
        }
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            peer.setLocalDescription(offer) { error in
                if let error {
                    continuation.resume(throwing: error)
                } else {
                    continuation.resume()
                }
            }
        }

        try await sendSignal(["type": "offer", "sdp": offer.sdp])
        let candidates = lock.withLock { () -> [RTCIceCandidate] in
            offerSent = true
            let candidates = localCandidates
            localCandidates.removeAll()
            return candidates
        }
        for candidate in candidates {
            enqueueLocalCandidate(candidate)
        }
    }

    private func waitUntilReady() async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            lock.lock()
            if let readyResult {
                lock.unlock()
                continuation.resume(with: readyResult)
                return
            }
            readyContinuation = continuation
            readyTimeoutTask = Task { [weak self] in
                do {
                    try await Task.sleep(nanoseconds: 20_000_000_000)
                    self?.completeReady(.failure(NativeSessionError.signalingTimeout))
                } catch {
                    // Ready or termination won the race.
                }
            }
            lock.unlock()
        }
    }

    private func completeReady(_ result: Result<Void, Error>) {
        lock.lock()
        guard readyResult == nil else {
            lock.unlock()
            return
        }
        readyResult = result
        let continuation = readyContinuation
        readyContinuation = nil
        let timeout = readyTimeoutTask
        readyTimeoutTask = nil
        if case .success = result { ready = true }
        lock.unlock()
        timeout?.cancel()
        continuation?.resume(with: result)
    }

    private func readSignaling() async {
        while !Task.isCancelled {
            let (socket, stopping) = lock.withLock { (self.socket, isStopping) }
            guard let socket, !stopping else { return }
            do {
                let message = try await socket.receive()
                let data: Data
                switch message {
                case let .string(text): data = Data(text.utf8)
                case let .data(bytes): data = bytes
                @unknown default: continue
                }
                guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any],
                      let type = object["type"] as? String else { continue }
                switch type {
                case "ready":
                    completeReady(.success(()))
                case "answer":
                    guard let sdp = object["sdp"] as? String, !sdp.isEmpty else {
                        fail(NativeSessionError.missingMediaAnswer.localizedDescription)
                        return
                    }
                    try await applyAnswer(sdp)
                case "candidate":
                    receiveRemoteCandidate(object["candidate"] as? [String: Any])
                case "ended":
                    let reason = object["reason"] as? String ?? "远端结束了会话。"
                    await finishFromRemote(reason)
                    return
                case "error":
                    let message = object["message"] as? String ?? "信令服务返回错误。"
                    await finishFromRemote(message)
                    return
                default:
                    continue
                }
            } catch {
                let stopping = lock.withLock { isStopping }
                if !stopping && !Task.isCancelled {
                    await finishFromRemote("信令连接中断：\(error.localizedDescription)")
                }
                return
            }
        }
    }

    private func applyAnswer(_ sdp: String) async throws {
        guard let peer = currentPeerConnection() else {
            throw NativeSessionError.peerConnectionUnavailable
        }
        let answer = RTCSessionDescription(type: .answer, sdp: sdp)
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            peer.setRemoteDescription(answer) { error in
                if let error {
                    continuation.resume(throwing: error)
                } else {
                    continuation.resume()
                }
            }
        }
        let (candidates, timeout) = lock.withLock { () -> ([RTCIceCandidate], Task<Void, Never>?) in
            remoteDescriptionSet = true
            let candidates = remoteCandidates
            remoteCandidates.removeAll()
            let timeout = answerTimeoutTask
            answerTimeoutTask = nil
            return (candidates, timeout)
        }
        timeout?.cancel()
        for candidate in candidates {
            addRemoteCandidate(candidate)
        }
        publishStatus("媒体应答已就绪，等待 ICE 连接…")
    }

    private func receiveRemoteCandidate(_ raw: [String: Any]?) {
        guard let raw,
              let candidateText = raw["candidate"] as? String,
              let lineValue = raw["sdpMLineIndex"] as? NSNumber else { return }
        let mid = raw["sdpMid"] as? String
        let candidate = RTCIceCandidate(
            sdp: candidateText,
            sdpMLineIndex: lineValue.int32Value,
            sdpMid: mid
        )
        lock.lock()
        let isReady = remoteDescriptionSet
        if !isReady { remoteCandidates.append(candidate) }
        lock.unlock()
        if isReady { addRemoteCandidate(candidate) }
    }

    private func addRemoteCandidate(_ candidate: RTCIceCandidate) {
        guard let peer = currentPeerConnection() else { return }
        peer.add(candidate) { [weak self] error in
            if let error {
                self?.publishStatus("远端 ICE 候选项被拒绝：\(error.localizedDescription)")
            }
        }
    }

    private func sendSignal(_ object: [String: Any]) async throws {
        let data = try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            signalSendQueue.async { [weak self] in
                guard let self else {
                    continuation.resume(throwing: CancellationError())
                    return
                }
                self.lock.lock()
                let socket = self.socket
                let stopping = self.isStopping
                self.lock.unlock()
                guard let socket, !stopping || object["type"] as? String == "leave" else {
                    continuation.resume(throwing: CancellationError())
                    return
                }
                socket.send(.string(String(decoding: data, as: UTF8.self))) { error in
                    if let error {
                        continuation.resume(throwing: error)
                    } else {
                        continuation.resume()
                    }
                }
            }
        }
    }

    private func enqueueLocalCandidate(_ candidate: RTCIceCandidate) {
        lock.lock()
        let shouldSend = offerSent && !isStopping
        lock.unlock()
        guard shouldSend else { return }
        var body: [String: Any] = [
            "candidate": candidate.sdp,
            "sdpMLineIndex": candidate.sdpMLineIndex,
        ]
        if let mid = candidate.sdpMid { body["sdpMid"] = mid }
        if let fragment = Self.usernameFragment(from: candidate.sdp) {
            body["usernameFragment"] = fragment
        }
        Task { [weak self] in
            do {
                try await self?.sendSignal(["type": "candidate", "candidate": body])
            } catch {
                self?.fail("发送 ICE 候选项失败：\(error.localizedDescription)")
            }
        }
    }

    private static func usernameFragment(from candidate: String) -> String? {
        let parts = candidate.split(whereSeparator: \.isWhitespace)
        guard let index = parts.firstIndex(of: "ufrag"), parts.indices.contains(index + 1) else { return nil }
        return String(parts[index + 1])
    }

    private func sendInput(_ data: Data, channel: RTCDataChannel? = nil) -> Bool {
        let target: RTCDataChannel?
        if let channel {
            target = channel
        } else {
            lock.lock()
            target = isStopping ? nil : controlChannel
            lock.unlock()
        }
        guard let target, target.readyState == .open else { return false }
        return target.sendData(RTCDataBuffer(data: data, isBinary: false))
    }

    private func readyControlChannel() -> RTCDataChannel? {
        lock.lock()
        defer { lock.unlock() }
        guard !isStopping, let channel = controlChannel, channel.readyState == .open else { return nil }
        return channel
    }

    private func sendEcho(action: (Int64, Int64) -> Data, on channel: RTCDataChannel) {
        lock.lock()
        let sequence = nextSequence
        nextSequence = nextSequence == Int64.max ? 1 : nextSequence + 1
        lock.unlock()
        let timestamp = Int64(Date().timeIntervalSince1970 * 1_000)
        _ = channel.sendData(RTCDataBuffer(
            data: action(sequence, timestamp),
            isBinary: true
        ))
    }

    private func receiveControlMessage(_ data: Data, from channel: RTCDataChannel) {
        guard let event = ControllerProtocol.decodeHostControl(data) else { return }
        switch event {
        case let .echoRequest(sequence, timestamp):
            _ = channel.sendData(RTCDataBuffer(
                data: ControllerProtocol.echoResponse(to: sequence, timestamp: timestamp),
                isBinary: true
            ))
        case .echoResponse:
            lock.lock()
            let notify = channel.label == "CONTROL_DATA_CHANNEL" && !receivedHandshakeResponse
            receivedHandshakeResponse = receivedHandshakeResponse || notify
            lock.unlock()
            if notify { DispatchQueue.main.async { [weak self] in self?.onHandshake?() } }
        case let .screenCatalog(currentScreenID, screens):
            lock.lock()
            hostScreens = Dictionary(screens.map { ($0.id, $0) }, uniquingKeysWith: { _, newest in newest })
            if !captureStateKnown && selectedScreenID == nil { selectedScreenID = currentScreenID }
            lock.unlock()
            publishSelectedVideoTrack()
        case let .captureChange(screenID):
            lock.lock()
            captureStateKnown = true
            selectedScreenID = screenID
            lock.unlock()
            publishSelectedVideoTrack()
        case .other:
            break
        }
    }

    private func publishSelectedVideoTrack() {
        lock.lock()
        let noActiveCapture = captureStateKnown && selectedScreenID == nil
        let selectedInfo = selectedScreenID.flatMap { hostScreens[$0] }
        let selected: RTCVideoTrack?
        if noActiveCapture {
            selected = nil
        } else if let info = selectedInfo,
                  info.videoTrackIndex >= 0,
                  let track = videoTracks[Int(info.videoTrackIndex)] {
            selected = track
        } else {
            selected = videoTracks.sorted(by: { $0.key < $1.key }).first?.value
        }
        lock.unlock()
        publishVideoTrack(selected)
    }

    private func publishVideoTrack(_ track: RTCVideoTrack?) {
        DispatchQueue.main.async { [weak self] in self?.onVideoTrack?(track) }
    }

    private func currentPeerConnection() -> RTCPeerConnection? {
        lock.lock()
        defer { lock.unlock() }
        return isStopping ? nil : peerConnection
    }

    private func publishStatus(_ value: String) {
        DispatchQueue.main.async { [weak self] in self?.onStatus?(value) }
    }

    private func fail(_ message: String) {
        let shouldNotify = lock.withLock { !isStopping && !terminalEventSent }
        guard shouldNotify else { return }
        Task { [weak self] in await self?.finishFromRemote(message) }
    }

    private func finishFromRemote(_ message: String) async {
        let snapshot = beginStopping()
        guard snapshot.shouldStop else { return }
        completeReady(.failure(NativeSessionError.invalidSignalingMessage))
        releaseKeys(snapshot.keys, on: snapshot.controlChannel)
        readyTimeoutTask?.cancel()
        answerTimeoutTask?.cancel()
        readerTask?.cancel()
        snapshot.controlChannel?.close()
        snapshot.textChannel?.close()
        snapshot.peerConnection?.close()
        if let socket = snapshot.socket { socket.cancel(with: .goingAway, reason: nil) }
        let shouldNotify = lock.withLock { () -> Bool in
            socket = nil
            controlChannel = nil
            textChannel = nil
            peerConnection = nil
            videoTracks.removeAll()
            let shouldNotify = !terminalEventSent
            terminalEventSent = true
            return shouldNotify
        }
        publishVideoTrack(nil)
        if shouldNotify { DispatchQueue.main.async { [weak self] in self?.onEnded?(message) } }
    }

    private struct StopSnapshot {
        let shouldStop: Bool
        let socket: URLSessionWebSocketTask?
        let peerConnection: RTCPeerConnection?
        let controlChannel: RTCDataChannel?
        let textChannel: RTCDataChannel?
        let keys: [UInt16]
    }

    private func beginStopping() -> StopSnapshot {
        lock.lock()
        guard !isStopping else {
            lock.unlock()
            return StopSnapshot(
                shouldStop: false,
                socket: nil,
                peerConnection: nil,
                controlChannel: nil,
                textChannel: nil,
                keys: []
            )
        }
        isStopping = true
        let snapshot = StopSnapshot(
            shouldStop: true,
            socket: socket,
            peerConnection: peerConnection,
            controlChannel: controlChannel,
            textChannel: textChannel,
            keys: Array(heldKeys)
        )
        heldKeys.removeAll()
        lock.unlock()
        return snapshot
    }

    private func releaseKeys(_ keys: [UInt16], on channel: RTCDataChannel?) {
        guard let channel, channel.readyState == .open else { return }
        for key in keys {
            guard let data = try? ControllerProtocol.key(key, down: false) else { continue }
            _ = channel.sendData(RTCDataBuffer(data: data, isBinary: false))
        }
    }

    private static let factory: RTCPeerConnectionFactory = {
        _ = RTCInitializeSSL()
        return RTCPeerConnectionFactory()
    }()

    // MARK: RTCPeerConnectionDelegate

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {}
    func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState) {
        let message: String
        switch newState {
        case .new: message = "媒体通道已创建，等待 ICE…"
        case .checking: message = "正在建立媒体连接…"
        case .connected, .completed: message = "媒体连接已建立，等待首帧…"
        case .disconnected: message = "媒体连接暂时中断，正在等待恢复…"
        case .failed: message = "ICE 连接失败，请检查服务端和网络配置。"
        case .closed: message = "媒体连接已关闭。"
        case .count: message = "媒体连接状态未知。"
        @unknown default: message = "媒体连接状态未知。"
        }
        publishStatus(message)
        if newState == .failed { fail(message) }
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) {
        lock.lock()
        let shouldQueue = !offerSent && !isStopping
        if shouldQueue { localCandidates.append(candidate) }
        lock.unlock()
        if !shouldQueue { enqueueLocalCandidate(candidate) }
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {
        dataChannel.delegate = self
        dataChannelDidChangeState(dataChannel)
    }

    func peerConnection(
        _ peerConnection: RTCPeerConnection,
        didAdd receiver: RTCRtpReceiver,
        streams: [RTCMediaStream]
    ) {
        if let audioTrack = receiver.track as? RTCAudioTrack {
            audioTrack.isEnabled = true
        }
        guard let videoTrack = receiver.track as? RTCVideoTrack else { return }
        lock.lock()
        let index = nextVideoIndex
        nextVideoIndex += 1
        videoTracks[index] = videoTrack
        lock.unlock()
        publishSelectedVideoTrack()
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove receiver: RTCRtpReceiver) {
        guard let videoTrack = receiver.track as? RTCVideoTrack else { return }
        lock.lock()
        videoTracks = videoTracks.filter { $0.value !== videoTrack }
        lock.unlock()
        publishSelectedVideoTrack()
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCPeerConnectionState) {
        if newState == .failed { fail("WebRTC 媒体连接失败。") }
    }

    // MARK: RTCDataChannelDelegate

    func dataChannelDidChangeState(_ dataChannel: RTCDataChannel) {
        lock.lock()
        if dataChannel.label == "CONTROL_DATA_CHANNEL" {
            if dataChannel.readyState == .open {
                controlChannel = dataChannel
            } else if dataChannel.readyState == .closed || dataChannel.readyState == .closing {
                controlChannel = nil
            }
        } else if dataChannel.label == "TEXT_DATA_CHANNEL" {
            if dataChannel.readyState == .open {
                textChannel = dataChannel
            } else if dataChannel.readyState == .closed || dataChannel.readyState == .closing {
                textChannel = nil
            }
        }
        let openedControl = dataChannel.label == "CONTROL_DATA_CHANNEL"
            && dataChannel.readyState == .open && !sentControllerEcho
        if openedControl { sentControllerEcho = true }
        let isOpen = dataChannel.readyState == .open
        lock.unlock()

        if openedControl {
            sendEcho(action: ControllerProtocol.echoRequest(sequence:timestamp:), on: dataChannel)
        }
        if isOpen {
            publishStatus(dataChannel.label == "CONTROL_DATA_CHANNEL"
                ? "控制通道已连接，等待主机握手…"
                : "文字通道已连接。")
        }
    }

    func dataChannel(_ dataChannel: RTCDataChannel, didReceiveMessageWith buffer: RTCDataBuffer) {
        receiveControlMessage(buffer.data, from: dataChannel)
    }
}
