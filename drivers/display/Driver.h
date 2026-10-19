#pragma once

#define NOMINMAX
#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
#include <ntstatus.h>
#include <bugcodes.h>
#include <wudfwdm.h>
#include <wdf.h>
#include <iddcx.h>

#include <dxgi1_5.h>
#include <d3d11_2.h>
#include <avrt.h>
#include <wrl.h>

#include <memory>
#include <queue>
#include <atomic>



namespace Microsoft
{
	namespace WRL
	{
		namespace Wrappers
		{
			// Adds a wrapper for thread handles to the existing set of WRL handle wrapper classes
			typedef HandleT<HandleTraits::HANDLENullTraits> Thread;
		}
	}
}

namespace Microsoft
{
	namespace IndirectDisp
	{
		struct VirtualMonitorMode {
			DWORD Width;
			DWORD Height;
			DWORD VSync;
		};

		/// <summary>
		/// Manages the creation and lifetime of a Direct3D render device.
		/// </summary>
		struct Direct3DDevice
		{
			Direct3DDevice(LUID AdapterLuid);
			Direct3DDevice();
			HRESULT Init();

			LUID AdapterLuid;
			Microsoft::WRL::ComPtr<IDXGIFactory5> DxgiFactory;
			Microsoft::WRL::ComPtr<IDXGIAdapter1> Adapter;
			Microsoft::WRL::ComPtr<ID3D11Device> Device;
			Microsoft::WRL::ComPtr<ID3D11DeviceContext> DeviceContext;
		};

		/// <summary>
		/// Manages a thread that consumes buffers from an indirect display swap-chain object.
		/// </summary>
		class SwapChainProcessor
		{
		public:
			SwapChainProcessor(IDDCX_SWAPCHAIN hSwapChain, std::shared_ptr<Direct3DDevice> Device, HANDLE NewFrameEvent);
			~SwapChainProcessor();

		private:
			static DWORD CALLBACK RunThread(LPVOID Argument);

			void Run();
			void RunCore();

			IDDCX_SWAPCHAIN m_hSwapChain;
			std::shared_ptr<Direct3DDevice> m_Device;
			HANDLE m_hAvailableBufferEvent;
			Microsoft::WRL::Wrappers::Thread m_hThread;
			Microsoft::WRL::Wrappers::Event m_hTerminateEvent;
		};

		class IndirectMonitorContext
		{
		public:
			UINT connectorId = 0;
			LUID adapterLuid{};
			UINT targetId = 0;
			GUID monitorGuid{};
            bool persistentFallback = false;
            WDFFILEOBJECT owner = nullptr;

			uint8_t* pEdidData = nullptr;
			VirtualMonitorMode preferredMode{};
			IDDCX_ADAPTER m_Adapter{};

			IndirectMonitorContext(_In_ IDDCX_MONITOR Monitor);
			virtual ~IndirectMonitorContext();

			void AssignSwapChain(const IDDCX_MONITOR& MonitorObject, const IDDCX_SWAPCHAIN& SwapChain, const LUID& RenderAdapter, const HANDLE& NewFrameEvent);
			void UnassignSwapChain();

			IDDCX_MONITOR GetMonitor() const;

		private:
			IDDCX_MONITOR m_Monitor;
			std::unique_ptr<SwapChainProcessor> m_ProcessingThread;
		} ;

		/// <summary>
		/// Provides a sample implementation of an indirect display driver.
		/// </summary>
		class IndirectDeviceContext
		{
		public:
			IndirectDeviceContext(_In_ WDFDEVICE WdfDevice);
			virtual ~IndirectDeviceContext();

			void InitAdapter();

			NTSTATUS SetRenderAdapter(const LUID& AdapterLuid);
            std::atomic<bool> adapterReady{false};

			NTSTATUS CreateMonitor(IndirectMonitorContext*& pMonitorContext, uint8_t* edidData, const GUID& containerId, const VirtualMonitorMode& preferredMode);

		protected:
			WDFDEVICE m_WdfDevice;
			IDDCX_ADAPTER m_Adapter;
		};
	}
}
