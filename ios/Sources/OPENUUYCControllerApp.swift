import SwiftUI

@main
struct OPENUUYCControllerApp: App {
    @StateObject private var model = ControllerModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            ControllerView(model: model)
                .task {
                    if model.selectedServer != nil && !model.isBusy {
                        await model.refreshHosts()
                    }
                }
                .onChange(of: scenePhase) { phase in
                    if phase == .background {
                        Task { await model.disconnect(fromBackground: true) }
                    }
                }
        }
    }
}
