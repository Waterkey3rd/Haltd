/* The kernel structures as FreeRTOS 10.3.1 declares them on a 32-bit port.
 * crates/engine/tests/freertos.rs builds matching images by hand. */
#include <stdint.h>

typedef unsigned long UBaseType_t;
typedef uint32_t TickType_t;
typedef uint32_t StackType_t;

struct xLIST;

struct xLIST_ITEM {
    TickType_t xItemValue;
    struct xLIST_ITEM* volatile pxNext;
    struct xLIST_ITEM* volatile pxPrevious;
    void* pvOwner;
    struct xLIST* volatile pvContainer;
};
typedef struct xLIST_ITEM ListItem_t;

struct xMINI_LIST_ITEM {
    TickType_t xItemValue;
    struct xLIST_ITEM* volatile pxNext;
    struct xLIST_ITEM* volatile pxPrevious;
};
typedef struct xMINI_LIST_ITEM MiniListItem_t;

typedef struct xLIST {
    volatile UBaseType_t uxNumberOfItems;
    ListItem_t* volatile pxIndex;
    MiniListItem_t xListEnd;
} List_t;

typedef struct tskTaskControlBlock {
    volatile StackType_t* pxTopOfStack;
    ListItem_t xStateListItem;
    ListItem_t xEventListItem;
    UBaseType_t uxPriority;
    StackType_t* pxStack;
    char pcTaskName[16];
    UBaseType_t uxBasePriority;
    volatile uint8_t ucNotifyState;
} tskTCB;
typedef tskTCB TCB_t;

TCB_t* volatile pxCurrentTCB;
List_t pxReadyTasksLists[7];
List_t xDelayedTaskList1;
List_t xDelayedTaskList2;
List_t xPendingReadyList;
List_t xSuspendedTaskList;
List_t xTasksWaitingTermination;
volatile UBaseType_t uxCurrentNumberOfTasks;

void entry(void) {
    TCB_t unused_tcb;
    (void)unused_tcb;
    for (;;) {
    }
}
