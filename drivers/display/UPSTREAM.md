# SudoVDA
Based on Microsoft Indirect Display Driver Sample. This creates a virtual display in Windows that acts and functions just like a real one. It's useful for streaming, virtual reality applications, recording, headless servers, etc. The benefit over a physical display is the ability to adjust resolutions and refresh rates beyond the physical displays capabilities. For example, this would enable the ability to stream a game from your home PC using game streaming software at 240hz at 8K while owning a 60hz 1080p monitor (~~unrealistic,~~ but explains the abilities well). For servers without displays, this enabled remote desktop and screen streaming to other systems as if there were a display installed. 

Supports emulating resolutions from **640 x 480** to **7680 x 4320 (8K)**, and refresh rates including **60hz, 75hz, 90hz, 120hz, 144hz, 165hz, 240hz, 480hz,** and **500hz.**

This project uses the official Windows Indirect Display Driver combined with the IddCx class extension driver. GitHub shows it's a fork of MTT's Virtual Display Driver, but it actually went a full rewrite half way. I keep the fork info for honoring the previous works done by MTT. 

## Notice

HDR is only fully supported on Windows 11 24H2. On 23H2 and lower, you'll try your luck to get the HDR toggle in Windows settings. HDR is not supported on Windows 10.

## Config

In Registry path `\HKEY_LOCAL_MACHINE\SOFTWARE\SudoMaker\SudoVDA` (create one if not exists):

- `gpuName`    [STRING]: The friendly name for the GPU which the virtual adapter connects to. Default unset, will choose the card with biggest videoram automatically.
- `maxMonitors` [DWORD]: Number of maximum virtual monitors can be created. Defaults to 10(decimal).
- `watchdog`    [DWORD]: Timeout in seconds for the watchdog to bark. Defaults to 3, set 0 to disable watchdog.
- `sdrBits`     [DWORD]: Bits for SDR mode. Defaults to 8(decimal)/8(HEX), set 10(decimal)/a(HEX) to enable SDR 10 bits, other values are ignored.
- `hdrBits`     [DWORD]: Bits for HDR mode. Defaults to 10(decimal)/a(HEX), set 12(decimal)/c(HEX) to enable HDR12 bits/HDR+, other values are ignored.

**NOTE**: After changing these values, you'll need to reload the driver or reboot your computer for them to take effect. Please note that if the driver is currently opened by something else, for example Apollo, it won't be able to reload, you'll need to quit the application before reloading the driver.

## License

MIT and CC0 or Public Domain (for changes I made, please consult Microsoft for their license), choose the least restrictive option.

## Disclaimer:

This software is provided "AS IS" with NO IMPLICIT OR EXPLICIT warranty. It's worth noting that while this software functioned without issues on my system, there is no guarantee that it will not impact your computer. It operates in User Mode, which reduces the likelihood of causing system instability, such as the Blue Screen of Death. However, exercise caution when using this software.

## Acknowledgements

Shoutout to **Roshkins** for the original repo:
https://github.com/roshkins/IddSampleDriver

Shoutout to **Baloukj** for the 8-bit / 10-bit support. (Also, first to push the new Microsoft Driver public!)
https://github.com/baloukj/IddSampleDriver

Shoutout to **Anakngtokwa** for assisting with finding driver sources.

**Microsoft** Indirect Display Driver/Sample (Driver code): 
https://github.com/microsoft/Windows-driver-samples/tree/master/video/IndirectDisplay

Thanks to **AKATrevorJay** https://github.com/akatrevorjay/edid-generator for the hi-res EDID.

Shoutout to **zjoasan** and **Bud** for the helper scripts and build support!

