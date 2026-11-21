import SwiftUI
import UIKit
import WebRTC

struct ControllerView: View {
    @ObservedObject var model: ControllerModel

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    statusCard

                    if let track = model.videoTrack {
                        RemoteDisplaySurface(
                            track: track,
                            videoSize: model.videoSize,
                            interactive: model.hasRenderedFrame,
                            onVideoSize: model.didRenderVideoFrame,
                            onPointerMove: { point in model.movePointer(x: point.x, y: point.y) },
                            onClick: model.click(at:)
                        )
                        .aspectRatio(videoAspect, contentMode: .fit)
                        .frame(maxHeight: 480)

                        inputControls
                    }

                    if let error = model.errorMessage {
                        errorCard(error)
                    }

                    pairingCard
                    pairedServersCard
                    hostsCard

                    Text("连接只使用上方配置的服务端地址。HTTPS/WSS 证书验证保持开启。")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .center)
                        .padding(.top, 2)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 14)
                .frame(maxWidth: 900)
                .frame(maxWidth: .infinity)
            }
            .background(Color(uiColor: .systemGroupedBackground))
            .navigationTitle("OpenUUYC 控制器")
            .navigationBarTitleDisplayMode(.inline)
        }
        .tint(.blue)
    }

    private var videoAspect: CGFloat {
        guard model.videoSize.width > 0, model.videoSize.height > 0 else { return 16.0 / 9.0 }
        return model.videoSize.width / model.videoSize.height
    }

    private var statusCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(model.connectionStatus)
                        .font(.headline)
                        .foregroundStyle(statusColor)
                    Text(model.connectionDetail)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 10)
                if model.canDisconnect {
                    Button(role: .destructive) {
                        Task { await model.disconnect() }
                    } label: {
                        Label("断开", systemImage: "xmark.circle.fill")
                    }
                    .buttonStyle(.bordered)
                }
            }
            if model.sessionIsActive {
                HStack(spacing: 8) {
                    statusBadge(model.hostHandshakeReady ? "主机握手完成" : "主机握手中", color: model.hostHandshakeReady ? .green : .orange)
                    if model.hasRenderedFrame {
                        statusBadge("H.264 / Opus", color: .blue)
                    }
                }
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color(uiColor: .secondarySystemGroupedBackground), in: RoundedRectangle(cornerRadius: 18))
    }

    private var statusColor: Color {
        if model.connectionStatus == "已连接" || model.connectionStatus == "已连接服务器" { return .green }
        if model.connectionStatus.contains("失败") || model.connectionStatus.contains("异常") { return .red }
        if model.sessionIsActive { return .orange }
        return .primary
    }

    private var inputControls: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label("键盘", systemImage: "keyboard")
                .font(.headline)
            HStack(alignment: .bottom, spacing: 8) {
                TextField("输入要发送到主机的文字", text: $model.textInput, axis: .vertical)
                    .lineLimit(1...3)
                    .textFieldStyle(.roundedBorder)
                    .submitLabel(.send)
                    .onSubmit(model.submitText)
                Button("发送", action: model.submitText)
                    .buttonStyle(.borderedProminent)
                    .disabled(model.textInput.isEmpty)
            }
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 8) {
                    keyButton("Esc", code: 27)
                    keyButton("Tab", code: 9)
                    keyButton("Enter", code: 13)
                    keyButton("⌫", code: 8)
                    keyButton("←", code: 37)
                    keyButton("↑", code: 38)
                    keyButton("↓", code: 40)
                    keyButton("→", code: 39)
                }
            }
            Text("轻点画面移动指针并单击；文字和按键通过控制通道发送。")
                .font(.footnote)
                .foregroundStyle(.secondary)
        }
        .padding(14)
        .background(Color(uiColor: .secondarySystemGroupedBackground), in: RoundedRectangle(cornerRadius: 16))
    }

    private func keyButton(_ title: String, code: UInt16) -> some View {
        Button(title) { model.tapKey(code) }
            .buttonStyle(.bordered)
            .accessibilityLabel("发送 \(title) 键")
    }

    private var pairingCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            sectionTitle("连接自建服务器", symbol: "link")
            TextField("HTTPS 服务端地址，例如 https://192.168.1.10:8443", text: $model.serverAddress)
                .keyboardType(.URL)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .textFieldStyle(.roundedBorder)
            DisclosureGroup("高级选项") {
                TextField("本机名称", text: $model.controllerName)
                    .textFieldStyle(.roundedBorder)
                SecureField("配对码（可选）", text: $model.pairCode)
                    .textContentType(.oneTimeCode)
                    .textFieldStyle(.roundedBorder)
            }
            Button {
                Task { await model.connectServer() }
            } label: {
                if model.isBusy && !model.sessionIsActive {
                    ProgressView().frame(maxWidth: .infinity)
                } else {
                    Label("连接服务器", systemImage: "link")
                        .frame(maxWidth: .infinity)
                }
            }
            .buttonStyle(.borderedProminent)
            .disabled(model.isBusy || model.sessionIsActive)
        }
        .padding(14)
        .background(Color(uiColor: .secondarySystemGroupedBackground), in: RoundedRectangle(cornerRadius: 16))
    }

    private var pairedServersCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                sectionTitle("已保存的服务器", symbol: "externaldrive.connected.to.line.below")
                Spacer()
                if model.selectedServer != nil {
                    Button {
                        Task { await model.refreshHosts() }
                    } label: {
                        Label("刷新", systemImage: "arrow.clockwise")
                    }
                    .disabled(model.isBusy || model.sessionIsActive)
                }
            }
            if model.pairedServers.isEmpty {
                Text("连接过的服务器会保存在这里。")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(model.pairedServers) { server in
                    HStack(spacing: 10) {
                        Button {
                            Task { await model.selectServer(server.origin) }
                        } label: {
                            VStack(alignment: .leading, spacing: 3) {
                                Text(server.name).font(.subheadline.weight(.semibold))
                                Text(server.origin)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                                    .lineLimit(1)
                            }
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .disabled(model.sessionIsActive || model.isBusy)
                        if model.selectedServerOrigin == server.origin {
                            Image(systemName: "checkmark.circle.fill")
                                .foregroundStyle(.blue)
                        }
                        Button(role: .destructive) {
                            Task { await model.forgetServer(server.origin) }
                        } label: {
                            Image(systemName: "trash")
                        }
                        .buttonStyle(.borderless)
                        .accessibilityLabel("忘记 \(server.name)")
                        .disabled(model.isBusy || (model.sessionIsActive && model.selectedServerOrigin == server.origin))
                    }
                    .padding(.vertical, 4)
                    if server.id != model.pairedServers.last?.id { Divider() }
                }
            }
        }
        .padding(14)
        .background(Color(uiColor: .secondarySystemGroupedBackground), in: RoundedRectangle(cornerRadius: 16))
    }

    private var hostsCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            sectionTitle("主机", symbol: "desktopcomputer")
            if model.selectedServer == nil {
                Text("连接服务器后显示主机。")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            } else if model.hosts.isEmpty {
                Text("暂无主机")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(model.hosts) { host in
                    VStack(alignment: .leading, spacing: 8) {
                        HStack {
                            VStack(alignment: .leading, spacing: 4) {
                                Text(host.name).font(.subheadline.weight(.semibold))
                                Text(host.id).font(.caption.monospaced()).foregroundStyle(.secondary)
                            }
                            Spacer()
                            VStack(alignment: .trailing, spacing: 4) {
                                statusBadge(host.online ? "在线" : "离线", color: host.online ? .green : .secondary)
                                statusBadge(host.controllable ? "允许控制" : "禁止控制", color: host.controllable ? .blue : .secondary)
                            }
                        }
                        Button {
                            model.selectedHostID = host.id
                            Task { await model.connect(to: host) }
                        } label: {
                            Label("连接主机", systemImage: "display")
                                .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.borderedProminent)
                        .disabled(!host.online || !host.controllable || model.isBusy || model.sessionIsActive)
                    }
                    .padding(.vertical, 5)
                    if host.id != model.hosts.last?.id { Divider() }
                }
            }
        }
        .padding(14)
        .background(Color(uiColor: .secondarySystemGroupedBackground), in: RoundedRectangle(cornerRadius: 16))
    }

    private func sectionTitle(_ title: String, symbol: String) -> some View {
        Label(title, systemImage: symbol)
            .font(.headline)
    }

    private func statusBadge(_ title: String, color: Color) -> some View {
        Text(title)
            .font(.caption.weight(.semibold))
            .foregroundStyle(color)
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(color.opacity(0.1), in: Capsule())
    }

    private func errorCard(_ message: String) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.red)
            VStack(alignment: .leading, spacing: 4) {
                Text("需要处理的问题").font(.subheadline.weight(.semibold))
                Text(message).font(.subheadline).fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
            Button { model.clearError() } label: {
                Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
            .accessibilityLabel("关闭错误信息")
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.red.opacity(0.08), in: RoundedRectangle(cornerRadius: 14))
    }
}

private struct RemoteDisplaySurface: View {
    let track: RTCVideoTrack
    let videoSize: CGSize
    let interactive: Bool
    let onVideoSize: (CGSize) -> Void
    let onPointerMove: (CGPoint) -> Void
    let onClick: (CGPoint) -> Void

    var body: some View {
        GeometryReader { geometry in
            let fitted = fittedRect(in: geometry.size)
            ZStack(alignment: .topLeading) {
                Color.black
                RTCMetalTrackView(track: track, onVideoSize: onVideoSize)
                    .frame(width: fitted.width, height: fitted.height)
                    .position(x: fitted.midX, y: fitted.midY)
                if !interactive {
                    Text("等待视频画面…")
                        .font(.subheadline)
                        .foregroundStyle(.white.opacity(0.8))
                        .position(x: geometry.size.width / 2, y: geometry.size.height / 2)
                }
            }
            .contentShape(Rectangle())
            .gesture(
                DragGesture(minimumDistance: 0, coordinateSpace: .local)
                    .onChanged { value in
                        if interactive, let point = normalized(value.location, in: geometry.size) {
                            onPointerMove(point)
                        }
                    }
                    .onEnded { value in
                        guard interactive,
                              hypot(value.translation.width, value.translation.height) < 12,
                              let point = normalized(value.location, in: geometry.size) else { return }
                        onClick(point)
                    }
            )
            .clipShape(RoundedRectangle(cornerRadius: 18))
            .overlay(alignment: .bottomLeading) {
                Text("触控移动指针 · 轻点单击")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .background(.black.opacity(0.55), in: Capsule())
                    .padding(10)
                    .allowsHitTesting(false)
            }
        }
        .background(Color.black, in: RoundedRectangle(cornerRadius: 18))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("远程主机屏幕")
    }

    private func fittedRect(in size: CGSize) -> CGRect {
        guard size.width > 0, size.height > 0 else { return .zero }
        let sourceWidth = videoSize.width > 0 ? videoSize.width : 16
        let sourceHeight = videoSize.height > 0 ? videoSize.height : 9
        let scale = min(size.width / sourceWidth, size.height / sourceHeight)
        let width = sourceWidth * scale
        let height = sourceHeight * scale
        return CGRect(x: (size.width - width) / 2, y: (size.height - height) / 2, width: width, height: height)
    }

    private func normalized(_ point: CGPoint, in size: CGSize) -> CGPoint? {
        let rect = fittedRect(in: size)
        guard rect.width > 0, rect.height > 0,
              rect.contains(point) else { return nil }
        return CGPoint(
            x: min(max((point.x - rect.minX) / rect.width, 0), 1),
            y: min(max((point.y - rect.minY) / rect.height, 0), 1)
        )
    }
}

private struct RTCMetalTrackView: UIViewRepresentable {
    let track: RTCVideoTrack
    let onVideoSize: (CGSize) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(onVideoSize: onVideoSize)
    }

    func makeUIView(context: Context) -> RTCMTLVideoView {
        let view = RTCMTLVideoView(frame: .zero)
        view.backgroundColor = .black
        view.videoContentMode = .scaleAspectFit
        view.delegate = context.coordinator
        context.coordinator.bind(track, to: view)
        return view
    }

    func updateUIView(_ view: RTCMTLVideoView, context: Context) {
        context.coordinator.onVideoSize = onVideoSize
        view.delegate = context.coordinator
        context.coordinator.bind(track, to: view)
    }

    static func dismantleUIView(_ view: RTCMTLVideoView, coordinator: Coordinator) {
        coordinator.unbind(from: view)
        view.delegate = nil
    }

    final class Coordinator: NSObject, RTCVideoViewDelegate {
        var onVideoSize: (CGSize) -> Void
        private weak var boundView: RTCMTLVideoView?
        private var boundTrack: RTCVideoTrack?

        init(onVideoSize: @escaping (CGSize) -> Void) {
            self.onVideoSize = onVideoSize
        }

        func bind(_ track: RTCVideoTrack, to view: RTCMTLVideoView) {
            if boundTrack !== track || boundView !== view {
                if let oldTrack = boundTrack, let oldView = boundView {
                    oldTrack.remove(oldView)
                }
                boundTrack = track
                boundView = view
                track.add(view)
            }
        }

        func unbind(from view: RTCMTLVideoView) {
            boundTrack?.remove(view)
            boundTrack = nil
            boundView = nil
        }

        func videoView(_ videoView: RTCVideoRenderer, didChangeVideoSize size: CGSize) {
            guard size.width > 0, size.height > 0 else { return }
            DispatchQueue.main.async { [weak self] in self?.onVideoSize(size) }
        }
    }
}
