# Farwing system identities

Per-profile names come from `src/product.rs`; the default profile uses the
plain names shown here.

| Item | Farwing value |
|------|---------------|
| Executable / launcher | `Farwing.exe` / `Farwing-launcher.exe` |
| Program directory | `%ProgramFiles%\Farwing\Farwing.exe` |
| Component staging | `%ProgramFiles%\FarwingComponents\package-*` |
| Machine data / service vault | `%ProgramData%\Farwing\host` |
| User data | `%LOCALAPPDATA%\Farwing` (profile `p`: `%LOCALAPPDATA%\Farwing\profiles\p`) |
| Runtime cache (launcher) | `%LOCALAPPDATA%\Farwing\runtime` |
| Credential services | `farwing[.p].session`, `farwing[.p].assist.<sha>`, `farwing[.p].wallpaper`, `farwing[.p].host.*`, `farwing[.p].viewing.*`, … |
| Kernel objects / pipes | `Local\Farwing[.p].*`, `Global\Farwing[.p].*`, `\\.\pipe\Farwing[.p].*` |
| Windows service | `FarwingHostService` ("Farwing Host Service") |
| Run value | `HKCU\...\Run\Farwing` |
| Uninstall key | `HKLM\...\Uninstall\Farwing` |
| Notification URI scheme | `farwing-notification` (profile `p`: `farwing-p-notification`) |
| AUMID | `Farwing.RemoteAccess` (profile `p`: `Farwing.RemoteAccess.p`) |
| Toast activator CLSID | `1c2fcbab-048b-4428-a60d-a118eee39d2a` |
| Shortcuts | `Farwing.lnk` |
| Display driver | `ROOT\FARWING_DISPLAY`, node `FarwingDisplay`, "Farwing Virtual Display Adapter", `FarwingDisplay.inf/.cat/.dll` |
| Display interface GUID | `69880c84-6cd7-4d27-af14-a08bad62808c` |
| Fallback monitor GUID | `65b02ee1-f206-41c8-8e3f-63b31949534f` |
| EDID | vendor `FWG`, product name `Farwing` |
| Input driver | `ROOT\FARWING_INPUT`, node `FarwingInput`, "Farwing Input", device `\\.\FarwingInput`, HID product ID `0x4657`, report magic `0x4657494e` (`FWIN`) |
| Audio driver | `ROOT\FARWING_AUDIO`, node `FarwingAudio`, service `FarwingAudio`, bridge `\\.\FarwingAudioBridge`, interfaces `FarwingSpeaker` / `FarwingMicrophone` |
| Audio circuit GUIDs | render `b6d42785-7971-42c9-8003-dd380402c60b`, capture `9715079d-b3a3-47ba-a7c1-7d77bd64282a` |
| Driver signing subject | `CN=Farwing Drivers` (pin derived from the bundled certificate) |
| Plugin ABI | `farwing_plugin_manifest_v1`, `farwing_plugin_query_v1`, `farwing_node_query_v1`, `farwing_video_node_query_v1`, metadata magic `FWPLMETA`, handshakes `Farwing plugin protocol 1` / `Farwing video protocol 1`, ids `farwing.effects.*` |
