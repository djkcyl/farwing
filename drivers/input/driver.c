/* Farwing UMDF2 input source; Windows supplies the kernel HID stack. */
#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
#include <ntstatus.h>
#include <wdf.h>
#include <vhf.h>
#include <bcrypt.h>
#include <stdio.h>
#include "reports.h"

typedef struct {
    VHFHANDLE Vhf;
    WDFIOTARGET Target;
    WDFWAITLOCK Lock;
    WDFTIMER Watchdog;
    WDFFILEOBJECT Owner;
    ULONGLONG Epoch, Sequence, Alive;
    UCHAR Buttons;
    BOOLEAN NeedsReset, Stopping;
} OU_CONTEXT;
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(OU_CONTEXT, Context);
DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD DeviceAdd;
EVT_WDF_DEVICE_PREPARE_HARDWARE Prepare;
EVT_WDF_DEVICE_RELEASE_HARDWARE Release;
EVT_WDF_DEVICE_D0_EXIT PowerExit;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL Control;
EVT_WDF_DEVICE_FILE_CREATE FileCreate;
EVT_WDF_FILE_CLEANUP FileCleanup;
EVT_WDF_OBJECT_CONTEXT_CLEANUP Cleanup;
EVT_WDF_TIMER Watchdog;

static NTSTATUS Checked(NTSTATUS status,const WCHAR *stage) {
    if(!NT_SUCCESS(status)) {
        WCHAR message[160];const WCHAR *strings[1];
        HANDLE source=RegisterEventSourceW(NULL,L"FarwingInput");
        _snwprintf_s(message,160,_TRUNCATE,L"%s: 0x%08lX",stage,(ULONG)status);
        strings[0]=message;
        if(source){ReportEventW(source,EVENTLOG_ERROR_TYPE,0,1,NULL,1,0,strings,NULL);DeregisterEventSource(source);}
    }
    return status;
}

static NTSTATUS Report(OU_CONTEXT *c,UCHAR *data,ULONG size) {
    HID_XFER_PACKET packet;NTSTATUS status;
    if(!c->Vhf)return STATUS_DEVICE_NOT_READY;
    packet.reportBuffer=data;packet.reportBufferLen=size;packet.reportId=data[0];
    status=VhfReadReportSubmit(c->Vhf,&packet);
    if(NT_SUCCESS(status)&&data[0]==5)c->Buttons=data[1];
    return status;
}
static NTSTATUS Neutral(OU_CONTEXT *c) {
    UCHAR keyboard[33]={1},consumer[2]={6},mouse[10]={5};
    NTSTATUS result=STATUS_SUCCESS,status;
    status=Report(c,keyboard,sizeof(keyboard));if(!NT_SUCCESS(status))result=status;
    status=Report(c,consumer,sizeof(consumer));if(!NT_SUCCESS(status))result=status;
    if(c->Buttons){status=Report(c,mouse,sizeof(mouse));if(!NT_SUCCESS(status))result=status;}
    return result;
}
static BOOLEAN Header(const UCHAR *p) {
    ULONG magic,version;memcpy(&magic,p,4);memcpy(&version,p+4,4);
    return magic==OU_MAGIC&&version==OU_VERSION;
}
static BOOLEAN ValidReport(const UCHAR *p,ULONG n) {
    switch(p[0]) {
    case 1:return n==33&&(p[1]&15)==0;
    case 6:return n==2&&(p[1]&128)==0;
    case 5:return n==10&&(p[1]&224)==0;
    default:return FALSE;
    }
}
VOID Control(WDFQUEUE queue,WDFREQUEST request,size_t output,size_t input,ULONG code) {
    OU_CONTEXT *c=Context(WdfIoQueueGetDevice(queue));
    UCHAR *p=NULL;ULONGLONG epoch=0,sequence=0;ULONG length=0;NTSTATUS status;
    UNREFERENCED_PARAMETER(output);
    if(input>284){WdfRequestComplete(request,STATUS_INVALID_BUFFER_SIZE);return;}
    status=WdfRequestRetrieveInputBuffer(request,8,(PVOID*)&p,NULL);
    if(!NT_SUCCESS(status)){WdfRequestComplete(request,status);return;}
    WdfWaitLockAcquire(c->Lock,NULL);
    status=STATUS_INVALID_PARAMETER;
    if(!c->Owner||c->Owner!=WdfRequestGetFileObject(request)){status=STATUS_ACCESS_DENIED;goto done;}
    if(code==OU_BEGIN&&input==16&&Header(p)) {
        memcpy(&epoch,p+8,8);if(!epoch)goto done;
        status=Neutral(c);
        if(NT_SUCCESS(status)){c->Epoch=epoch;c->Sequence=0;c->Alive=GetTickCount64();c->NeedsReset=FALSE;}
    } else if((code==OU_RESET||code==OU_ALIVE)&&input==8) {
        memcpy(&epoch,p,8);
        if(!epoch||epoch!=c->Epoch){status=STATUS_ACCESS_DENIED;goto done;}
        status=code==OU_RESET?Neutral(c):STATUS_SUCCESS;
        if(NT_SUCCESS(status))c->Alive=GetTickCount64();
    } else if(code==OU_SUBMIT&&input>=29&&Header(p)) {
        memcpy(&epoch,p+8,8);memcpy(&sequence,p+16,8);memcpy(&length,p+24,4);
        if(!epoch||epoch!=c->Epoch||!sequence||sequence<=c->Sequence){status=STATUS_ACCESS_DENIED;goto done;}
        if(length!=input-28||!ValidReport(p+28,length))goto done;
        c->Sequence=sequence;
        status=Report(c,p+28,length);
        if(NT_SUCCESS(status))c->Alive=GetTickCount64();
    }
done:
    WdfWaitLockRelease(c->Lock);WdfRequestComplete(request,status);
}
VOID FileCreate(WDFDEVICE device,WDFREQUEST request,WDFFILEOBJECT file) {
    OU_CONTEXT *c=Context(device);NTSTATUS status;
    WdfWaitLockAcquire(c->Lock,NULL);
    status=!file?STATUS_INVALID_HANDLE:(c->Owner?STATUS_SHARING_VIOLATION:STATUS_SUCCESS);
    if(NT_SUCCESS(status))c->Owner=file;
    WdfWaitLockRelease(c->Lock);WdfRequestComplete(request,status);
}
VOID FileCleanup(WDFFILEOBJECT file) {
    OU_CONTEXT *c=Context(WdfFileObjectGetDevice(file));
    WdfWaitLockAcquire(c->Lock,NULL);
    if(c->Owner==file){c->NeedsReset=!NT_SUCCESS(Neutral(c));c->Owner=NULL;c->Epoch=0;c->Sequence=0;}
    WdfWaitLockRelease(c->Lock);
}
VOID Watchdog(WDFTIMER timer) {
    OU_CONTEXT *c=Context(WdfTimerGetParentObject(timer));BOOLEAN restart;
    WdfWaitLockAcquire(c->Lock,NULL);
    if(c->NeedsReset||(c->Epoch&&GetTickCount64()-c->Alive>500)){
        c->NeedsReset=!NT_SUCCESS(Neutral(c));c->Epoch=0;c->Sequence=0;
    }
    restart=!c->Stopping;WdfWaitLockRelease(c->Lock);
    if(restart)WdfTimerStart(timer,WDF_REL_TIMEOUT_IN_MS(100));
}
NTSTATUS PowerExit(WDFDEVICE device,WDF_POWER_DEVICE_STATE target) {
    OU_CONTEXT *c=Context(device);UNREFERENCED_PARAMETER(target);
    WdfWaitLockAcquire(c->Lock,NULL);
    c->NeedsReset=!NT_SUCCESS(Neutral(c));c->Epoch=0;c->Sequence=0;
    WdfWaitLockRelease(c->Lock);return STATUS_SUCCESS;
}
static VOID DeleteSource(OU_CONTEXT *c) {
    VHFHANDLE vhf;WDFIOTARGET target;
    WdfWaitLockAcquire(c->Lock,NULL);
    if(c->Vhf)Neutral(c);
    vhf=c->Vhf;target=c->Target;c->Vhf=NULL;c->Target=NULL;c->Epoch=0;
    WdfWaitLockRelease(c->Lock);
    if(vhf)VhfDelete(vhf,TRUE);
    if(target)WdfObjectDelete(target);
}
static NTSTATUS InputContainer(WDFDEVICE device,GUID *container) {
    WDFKEY key;NTSTATUS status;ULONG size=0,type=0;
    const GUID empty={0};
    DECLARE_CONST_UNICODE_STRING(name,L"FarwingInputContainerId");
    status=WdfDeviceOpenRegistryKey(device,PLUGPLAY_REGKEY_DEVICE|WDF_REGKEY_DEVICE_SUBKEY,KEY_READ|KEY_SET_VALUE,WDF_NO_OBJECT_ATTRIBUTES,&key);
    if(!NT_SUCCESS(status))return status;
    status=WdfRegistryQueryValue(key,&name,sizeof(*container),container,&size,&type);
    if(status==STATUS_OBJECT_NAME_NOT_FOUND) {
        status=BCryptGenRandom(NULL,(PUCHAR)container,sizeof(*container),BCRYPT_USE_SYSTEM_PREFERRED_RNG);
        if(NT_SUCCESS(status)) {
            container->Data3=(container->Data3&0x0fff)|0x4000;
            container->Data4[0]=(container->Data4[0]&0x3f)|0x80;
            status=WdfRegistryAssignValue(key,&name,REG_BINARY,sizeof(*container),container);
        }
    } else if(NT_SUCCESS(status)&&(type!=REG_BINARY||size!=sizeof(*container)||!memcmp(container,&empty,sizeof(empty)))) {
        status=STATUS_INVALID_PARAMETER;
    }
    WdfRegistryClose(key);return status;
}
NTSTATUS Prepare(WDFDEVICE device,WDFCMRESLIST raw,WDFCMRESLIST translated) {
    OU_CONTEXT *c=Context(device);WDF_OBJECT_ATTRIBUTES attributes;
    WDF_IO_TARGET_OPEN_PARAMS open;VHF_CONFIG config;NTSTATUS status;
    UNREFERENCED_PARAMETER(raw);UNREFERENCED_PARAMETER(translated);
    WDF_OBJECT_ATTRIBUTES_INIT(&attributes);attributes.ParentObject=device;
    status=Checked(WdfIoTargetCreate(device,&attributes,&c->Target),L"Create lower target");if(!NT_SUCCESS(status))return status;
    WDF_IO_TARGET_OPEN_PARAMS_INIT_OPEN_BY_FILE(&open,NULL);
    status=Checked(WdfIoTargetOpen(c->Target,&open),L"Open lower target");
    if(!NT_SUCCESS(status)){DeleteSource(c);return status;}
    VHF_CONFIG_INIT(&config,WdfIoTargetWdmGetTargetFileHandle(c->Target),sizeof(OuDescriptor),OuDescriptor);
    config.VendorID=0;config.ProductID=0x4657;config.VersionNumber=1;
    /* An unspecified container inherits the computer's internal-device group.
       Windows classifies internal relative mice as legacy touchpads on laptops,
       suppressing left clicks during typing. Own one persistent container for
       this composite input device, independent of the physical touchpad. */
    status=Checked(InputContainer(device,&config.ContainerID),L"Prepare input container");
    if(!NT_SUCCESS(status)){DeleteSource(c);return status;}
    status=Checked(VhfCreate(&config,&c->Vhf),L"Create VHF source");
    if(NT_SUCCESS(status))status=Checked(VhfStart(c->Vhf),L"Start VHF source");
    if(!NT_SUCCESS(status))DeleteSource(c);
    return status;
}
NTSTATUS Release(WDFDEVICE device,WDFCMRESLIST translated) {
    UNREFERENCED_PARAMETER(translated);DeleteSource(Context(device));return STATUS_SUCCESS;
}
VOID Cleanup(WDFOBJECT object) {
    OU_CONTEXT *c=Context(object);
    if(!c->Lock)return;
    WdfWaitLockAcquire(c->Lock,NULL);c->Stopping=TRUE;WdfWaitLockRelease(c->Lock);
    if(c->Watchdog)WdfTimerStop(c->Watchdog,TRUE);
    DeleteSource(c);
}
NTSTATUS DeviceAdd(WDFDRIVER driver,PWDFDEVICE_INIT init) {
    WDFDEVICE device;OU_CONTEXT *c;WDF_OBJECT_ATTRIBUTES attributes;
    WDF_PNPPOWER_EVENT_CALLBACKS power;WDF_FILEOBJECT_CONFIG files;
    WDF_IO_QUEUE_CONFIG queue;WDF_TIMER_CONFIG timer;NTSTATUS status;
    DECLARE_CONST_UNICODE_STRING(link,L"\\DosDevices\\Global\\FarwingInput");
    UNREFERENCED_PARAMETER(driver);
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&power);
    power.EvtDevicePrepareHardware=Prepare;power.EvtDeviceReleaseHardware=Release;power.EvtDeviceD0Exit=PowerExit;
    WdfDeviceInitSetPnpPowerEventCallbacks(init,&power);
    WDF_FILEOBJECT_CONFIG_INIT(&files,FileCreate,WDF_NO_EVENT_CALLBACK,FileCleanup);
    WDF_OBJECT_ATTRIBUTES_INIT(&attributes);attributes.ExecutionLevel=WdfExecutionLevelPassive;
    WdfDeviceInitSetFileObjectConfig(init,&files,&attributes);
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes,OU_CONTEXT);
    attributes.ExecutionLevel=WdfExecutionLevelPassive;attributes.EvtCleanupCallback=Cleanup;
    status=Checked(WdfDeviceCreate(&init,&attributes,&device),L"Create device");if(!NT_SUCCESS(status))return status;
    c=Context(device);
    WDF_OBJECT_ATTRIBUTES_INIT(&attributes);attributes.ParentObject=device;
    status=Checked(WdfWaitLockCreate(&attributes,&c->Lock),L"Create state lock");if(!NT_SUCCESS(status))return status;
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queue,WdfIoQueueDispatchSequential);queue.EvtIoDeviceControl=Control;
    status=Checked(WdfIoQueueCreate(device,&queue,WDF_NO_OBJECT_ATTRIBUTES,NULL),L"Create queue");if(!NT_SUCCESS(status))return status;
    status=Checked(WdfDeviceCreateSymbolicLink(device,&link),L"Create device link");if(!NT_SUCCESS(status))return status;
    WDF_TIMER_CONFIG_INIT(&timer,Watchdog);timer.AutomaticSerialization=FALSE;
    WDF_OBJECT_ATTRIBUTES_INIT(&attributes);attributes.ParentObject=device;attributes.ExecutionLevel=WdfExecutionLevelPassive;
    status=Checked(WdfTimerCreate(&timer,&attributes,&c->Watchdog),L"Create watchdog");if(!NT_SUCCESS(status))return status;
    WdfTimerStart(c->Watchdog,WDF_REL_TIMEOUT_IN_MS(100));return STATUS_SUCCESS;
}
NTSTATUS DriverEntry(PDRIVER_OBJECT driver,PUNICODE_STRING path) {
    WDF_DRIVER_CONFIG config;WDF_DRIVER_CONFIG_INIT(&config,DeviceAdd);
    return Checked(WdfDriverCreate(driver,path,WDF_NO_OBJECT_ATTRIBUTES,&config,WDF_NO_HANDLE),L"Create driver");
}
