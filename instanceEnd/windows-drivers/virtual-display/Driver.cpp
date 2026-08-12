// The IddCx control flow is adapted from Microsoft's IndirectDisplay sample.
// This source is an HLK/Verifier development baseline, not a signed release.

#define NOMINMAX
#include <windows.h>
#include <wudfwdm.h>
#include <wdf.h>
#include <iddcx.h>
#include <d3d11.h>
#include <dxgi1_5.h>
#include <wrl.h>

#include <array>
#include <memory>

using Microsoft::WRL::ComPtr;

namespace {

constexpr std::array<SIZE, 4> kModes = {{{1024, 768}, {1280, 720}, {1600, 900}, {1920, 1080}}};

struct DeviceContext {
    IDDCX_ADAPTER adapter;
};
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DeviceContext, GetDeviceContext);

struct AdapterContext {
    WDFDEVICE device;
};
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(AdapterContext, GetAdapterContext);

struct MonitorContext {
    IDDCX_SWAPCHAIN swapchain;
    LUID render_adapter;
    HANDLE next_surface;
    HANDLE stop_event;
    HANDLE worker;
};
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(MonitorContext, GetMonitorContext);

void FillSignalInfo(
    DISPLAYCONFIG_VIDEO_SIGNAL_INFO& signal,
    UINT width,
    UINT height,
    UINT refresh_rate,
    bool monitor_mode)
{
    signal.totalSize.cx = signal.activeSize.cx = width;
    signal.totalSize.cy = signal.activeSize.cy = height;
    signal.vSyncFreq.Numerator = refresh_rate;
    signal.vSyncFreq.Denominator = 1;
    signal.hSyncFreq.Numerator = refresh_rate * height;
    signal.hSyncFreq.Denominator = 1;
    signal.pixelRate = static_cast<UINT64>(refresh_rate) * width * height;
    signal.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    signal.AdditionalSignalInfo.videoStandard = 255;
    signal.AdditionalSignalInfo.vSyncFreqDivider = monitor_mode ? 0 : 1;
}

IDDCX_MONITOR_MODE MakeMonitorMode(const SIZE& size)
{
    IDDCX_MONITOR_MODE mode = {};
    mode.Size = sizeof(mode);
    mode.Origin = IDDCX_MONITOR_MODE_ORIGIN_DRIVER;
    FillSignalInfo(
        mode.MonitorVideoSignalInfo,
        static_cast<UINT>(size.cx),
        static_cast<UINT>(size.cy),
        60,
        true);
    return mode;
}

IDDCX_TARGET_MODE MakeTargetMode(const SIZE& size)
{
    IDDCX_TARGET_MODE mode = {};
    mode.Size = sizeof(mode);
    FillSignalInfo(
        mode.TargetVideoSignalInfo.targetVideoSignalInfo,
        static_cast<UINT>(size.cx),
        static_cast<UINT>(size.cy),
        60,
        false);
    return mode;
}

DWORD WINAPI ProcessSwapChain(void* argument)
{
    auto* context = static_cast<MonitorContext*>(argument);
    ComPtr<IDXGIFactory5> factory;
    if (FAILED(CreateDXGIFactory2(0, IID_PPV_ARGS(&factory)))) {
        return 1;
    }

    ComPtr<IDXGIAdapter1> adapter;
    if (FAILED(factory->EnumAdapterByLuid(context->render_adapter, IID_PPV_ARGS(&adapter)))) {
        return 1;
    }

    IDARG_IN_SWAPCHAINSETDEVICE set_device = {};
    ComPtr<ID3D11Device> d3d_device;
    ComPtr<ID3D11DeviceContext> d3d_context;
    D3D_FEATURE_LEVEL feature_level;
    if (FAILED(D3D11CreateDevice(
            adapter.Get(),
            D3D_DRIVER_TYPE_UNKNOWN,
            nullptr,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            nullptr,
            0,
            D3D11_SDK_VERSION,
            &d3d_device,
            &feature_level,
            &d3d_context))) {
        return 1;
    }
    ComPtr<IDXGIDevice> dxgi_device;
    if (FAILED(d3d_device.As(&dxgi_device))) {
        return 1;
    }
    set_device.pDevice = dxgi_device.Get();
    if (FAILED(IddCxSwapChainSetDevice(context->swapchain, &set_device))) {
        return 1;
    }

    HANDLE wait_handles[] = {context->stop_event, context->next_surface};
    for (;;) {
        IDARG_OUT_RELEASEANDACQUIREBUFFER buffer = {};
        const HRESULT result = IddCxSwapChainReleaseAndAcquireBuffer(context->swapchain, &buffer);
        if (result == E_PENDING) {
            const DWORD wait = WaitForMultipleObjects(ARRAYSIZE(wait_handles), wait_handles, FALSE, 16);
            if (wait == WAIT_OBJECT_0) {
                break;
            }
            if (wait == WAIT_OBJECT_0 + 1 || wait == WAIT_TIMEOUT) {
                continue;
            }
            break;
        }
        if (FAILED(result)) {
            break;
        }

        ComPtr<IDXGIResource> surface;
        surface.Attach(buffer.MetaData.pSurface);
        surface.Reset();
        if (FAILED(IddCxSwapChainFinishedProcessingFrame(context->swapchain))) {
            break;
        }
    }
    return 0;
}

void StopSwapChain(MonitorContext* context)
{
    if (context->stop_event != nullptr) {
        SetEvent(context->stop_event);
    }
    if (context->worker != nullptr) {
        WaitForSingleObject(context->worker, INFINITE);
        CloseHandle(context->worker);
    }
    if (context->stop_event != nullptr) {
        CloseHandle(context->stop_event);
    }
    context->worker = nullptr;
    context->stop_event = nullptr;
    context->swapchain = nullptr;
}

EVT_WDF_OBJECT_CONTEXT_CLEANUP MonitorCleanup;
void MonitorCleanup(WDFOBJECT object)
{
    StopSwapChain(GetMonitorContext(object));
}

EVT_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES GetDefaultMonitorModes;
NTSTATUS GetDefaultMonitorModes(
    IDDCX_MONITOR,
    const IDARG_IN_GETDEFAULTDESCRIPTIONMODES* input,
    IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* output)
{
    output->DefaultMonitorModeBufferOutputCount = static_cast<UINT>(kModes.size());
    output->PreferredMonitorModeIdx = static_cast<UINT>(kModes.size() - 1);
    if (input->DefaultMonitorModeBufferInputCount == 0) {
        return STATUS_SUCCESS;
    }
    if (input->DefaultMonitorModeBufferInputCount < static_cast<UINT>(kModes.size())) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    for (size_t index = 0; index < kModes.size(); ++index) {
        input->pDefaultMonitorModes[index] = MakeMonitorMode(kModes[index]);
    }
    return STATUS_SUCCESS;
}

EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES QueryTargetModes;
NTSTATUS QueryTargetModes(
    IDDCX_MONITOR,
    const IDARG_IN_QUERYTARGETMODES* input,
    IDARG_OUT_QUERYTARGETMODES* output)
{
    output->TargetModeBufferOutputCount = static_cast<UINT>(kModes.size());
    if (input->TargetModeBufferInputCount == 0) {
        return STATUS_SUCCESS;
    }
    if (input->TargetModeBufferInputCount < static_cast<UINT>(kModes.size())) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    for (size_t index = 0; index < kModes.size(); ++index) {
        input->pTargetModes[index] = MakeTargetMode(kModes[index]);
    }
    return STATUS_SUCCESS;
}

EVT_IDD_CX_ADAPTER_COMMIT_MODES CommitModes;
NTSTATUS CommitModes(IDDCX_ADAPTER, const IDARG_IN_COMMITMODES*)
{
    return STATUS_SUCCESS;
}

EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN AssignSwapChain;
NTSTATUS AssignSwapChain(IDDCX_MONITOR monitor, const IDARG_IN_SETSWAPCHAIN* input)
{
    auto* context = GetMonitorContext(monitor);
    StopSwapChain(context);
    context->swapchain = input->hSwapChain;
    context->render_adapter = input->RenderAdapterLuid;
    context->next_surface = input->hNextSurfaceAvailable;
    context->stop_event = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    if (context->stop_event == nullptr) {
        WdfObjectDelete(input->hSwapChain);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    context->worker = CreateThread(nullptr, 0, ProcessSwapChain, context, 0, nullptr);
    if (context->worker == nullptr) {
        StopSwapChain(context);
        WdfObjectDelete(input->hSwapChain);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    return STATUS_SUCCESS;
}

EVT_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN UnassignSwapChain;
NTSTATUS UnassignSwapChain(IDDCX_MONITOR monitor)
{
    StopSwapChain(GetMonitorContext(monitor));
    return STATUS_SUCCESS;
}

EVT_IDD_CX_ADAPTER_INIT_FINISHED AdapterInitFinished;
NTSTATUS AdapterInitFinished(
    IDDCX_ADAPTER adapter,
    const IDARG_IN_ADAPTER_INIT_FINISHED* input)
{
    if (!NT_SUCCESS(input->AdapterInitStatus)) {
        return STATUS_SUCCESS;
    }

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, MonitorContext);
    attributes.EvtCleanupCallback = MonitorCleanup;

    IDDCX_MONITOR_INFO monitor_info = {};
    monitor_info.Size = sizeof(monitor_info);
    monitor_info.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL;
    monitor_info.ConnectorIndex = 0;
    monitor_info.MonitorDescription.Size = sizeof(monitor_info.MonitorDescription);
    monitor_info.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    monitor_info.MonitorDescription.DataSize = 0;
    monitor_info.MonitorDescription.pData = nullptr;
    monitor_info.MonitorContainerId = {0x7a925360, 0xee02, 0x4f3d, {0xa3, 0xdc, 0xee, 0x69, 0x72, 0xb7, 0x8e, 0xf1}};

    IDARG_IN_MONITORCREATE create = {};
    create.ObjectAttributes = &attributes;
    create.pMonitorInfo = &monitor_info;
    IDARG_OUT_MONITORCREATE created = {};
    NTSTATUS status = IddCxMonitorCreate(adapter, &create, &created);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    auto* monitor_context = GetMonitorContext(created.MonitorObject);
    *monitor_context = {};
    IDARG_OUT_MONITORARRIVAL arrival = {};
    return IddCxMonitorArrival(created.MonitorObject, &arrival);
}

EVT_WDF_DEVICE_D0_ENTRY DeviceD0Entry;
NTSTATUS DeviceD0Entry(WDFDEVICE device, WDF_POWER_DEVICE_STATE)
{
    auto* context = GetDeviceContext(device);
    if (context->adapter != nullptr) {
        return STATUS_SUCCESS;
    }

    IDDCX_ADAPTER_CAPS capabilities = {};
    capabilities.Size = sizeof(capabilities);
    capabilities.MaxMonitorsSupported = 1;
    capabilities.EndPointDiagnostics.Size = sizeof(capabilities.EndPointDiagnostics);
    capabilities.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    capabilities.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;
    capabilities.EndPointDiagnostics.pEndPointFriendlyName = L"Operation Monitoring Virtual Display";
    capabilities.EndPointDiagnostics.pEndPointManufacturerName = L"Operation Monitoring";
    capabilities.EndPointDiagnostics.pEndPointModelName = L"OM Headless Display";
    IDDCX_ENDPOINT_VERSION version = {};
    version.Size = sizeof(version);
    version.MajorVer = 1;
    capabilities.EndPointDiagnostics.pFirmwareVersion = &version;
    capabilities.EndPointDiagnostics.pHardwareVersion = &version;

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, AdapterContext);
    IDARG_IN_ADAPTER_INIT initialize = {};
    initialize.WdfDevice = device;
    initialize.pCaps = &capabilities;
    initialize.ObjectAttributes = &attributes;
    IDARG_OUT_ADAPTER_INIT initialized = {};
    NTSTATUS status = IddCxAdapterInitAsync(&initialize, &initialized);
    if (NT_SUCCESS(status)) {
        context->adapter = initialized.AdapterObject;
        GetAdapterContext(initialized.AdapterObject)->device = device;
    }
    return status;
}

EVT_WDF_DRIVER_DEVICE_ADD DeviceAdd;
NTSTATUS DeviceAdd(WDFDRIVER, PWDFDEVICE_INIT device_init)
{
    WDF_PNPPOWER_EVENT_CALLBACKS power_callbacks;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&power_callbacks);
    power_callbacks.EvtDeviceD0Entry = DeviceD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(device_init, &power_callbacks);

    IDD_CX_CLIENT_CONFIG idd_config;
    IDD_CX_CLIENT_CONFIG_INIT(&idd_config);
    idd_config.EvtIddCxAdapterInitFinished = AdapterInitFinished;
    idd_config.EvtIddCxMonitorGetDefaultDescriptionModes = GetDefaultMonitorModes;
    idd_config.EvtIddCxMonitorQueryTargetModes = QueryTargetModes;
    idd_config.EvtIddCxAdapterCommitModes = CommitModes;
    idd_config.EvtIddCxMonitorAssignSwapChain = AssignSwapChain;
    idd_config.EvtIddCxMonitorUnassignSwapChain = UnassignSwapChain;
    NTSTATUS status = IddCxDeviceInitConfig(device_init, &idd_config);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, DeviceContext);
    WDFDEVICE device = nullptr;
    status = WdfDeviceCreate(&device_init, &attributes, &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    GetDeviceContext(device)->adapter = nullptr;
    return IddCxDeviceInitialize(device);
}

} // namespace

extern "C" BOOL WINAPI DllMain(HINSTANCE, UINT, LPVOID)
{
    return TRUE;
}

extern "C" DRIVER_INITIALIZE DriverEntry;
extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT driver_object, PUNICODE_STRING registry_path)
{
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, DeviceAdd);
    return WdfDriverCreate(
        driver_object,
        registry_path,
        WDF_NO_OBJECT_ATTRIBUTES,
        &config,
        WDF_NO_HANDLE);
}
