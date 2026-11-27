// The iPhone Duo's hinge, from inside its simulator. Runs in the simulated
// iOS (`simctl spawn`) and reads one hinge angle in degrees per line on
// stdin: 0 folded shut, 180 open flat.
//
// iOS learns the hinge angle from locationd, which listens for vendor-defined
// HID events on usage page 0xff61, usage 0x5b: a serialized dictionary naming
// the control that moved. Xcode's Device Hub sends the same dictionary through
// CoreDevice to dtuhidd, which posts it from a HID virtual service; this posts
// it from its own. Built for the iOS Simulator by build.rs.

#import <Foundation/Foundation.h>
#import <objc/message.h>
#include <dlfcn.h>
#include <mach/mach_time.h>
#include <stdio.h>
#include <stdlib.h>

extern CFDataRef IOCFSerialize(CFTypeRef object, uint32_t options);
static const uint32_t kSerializeToBinary = 1;
static const uint16_t kAVPUsagePage = 0xff61, kAVPUsage = 0x5b;

/// Answers HID's questions about the service: what it is and what it sends.
@interface LeafHingeService : NSObject
@end

@implementation LeafHingeService
- (id)propertyForKey:(NSString *)key forService:(id)service {
    NSDictionary *properties = @{
        @"PrimaryUsagePage": @(kAVPUsagePage),
        @"PrimaryUsage": @(kAVPUsage),
        @"DeviceUsagePairs": @[ @{ @"DeviceUsagePage": @(kAVPUsagePage), @"DeviceUsage": @(kAVPUsage) } ],
        @"Product": @"leaf iPhone Duo hinge",
        @"Transport": @"Virtual",
    };
    return properties[key];
}
- (BOOL)setProperty:(id)value forKey:(NSString *)key forService:(id)service { return YES; }
- (void)notification:(NSInteger)type withProperty:(id)property forService:(id)service {}
- (id)copyEventMatching:(id)matching forService:(id)service { return nil; }
- (BOOL)setOutputEvent:(id)event forService:(id)service { return YES; }
@end

static id service;

static void send(double degrees) {
    NSDictionary *control = @{
        @"provider": @"com.apple.Virtualization.VirtualMachines",
        @"source": @"hinge-slider-control",
        @"type": @"range",
        @"value": @(MAX(0, MIN(180, degrees))),
    };
    // Kept alive to the end: HID reads `data.bytes` after its last mention.
    __attribute__((objc_precise_lifetime)) NSData *data =
        CFBridgingRelease(IOCFSerialize((__bridge CFTypeRef)control, kSerializeToBinary));
    if (data == nil) {
        fprintf(stderr, "leaf-duo-hinge: couldn't serialize the hinge angle %.1f; the hinge didn't move.\n", degrees);
        return;
    }
    id event = ((id (*)(id, SEL, uint64_t, uint16_t, uint16_t, uint32_t, const void *, uint32_t, uint32_t))objc_msgSend)(
        objc_getClass("HIDEvent"), sel_registerName("vendorDefinedEvent:usagePage:usage:version:data:length:options:"),
        mach_absolute_time(), kAVPUsagePage, kAVPUsage, 0, data.bytes, (uint32_t)data.length, 0);
    BOOL sent = event != nil && ((BOOL (*)(id, SEL, id))objc_msgSend)(service, sel_registerName("dispatchEvent:"), event);
    if (!sent) fprintf(stderr, "leaf-duo-hinge: iOS refused the hinge angle %.1f; the hinge didn't move.\n", degrees);
}

int main(void) {
    @autoreleasepool {
        if (dlopen("/System/Library/PrivateFrameworks/HID.framework/HID", RTLD_NOW) == NULL || !objc_getClass("HIDVirtualEventService")) {
            fprintf(stderr, "leaf-duo-hinge: this iOS runtime has no HIDVirtualEventService, so leaf can't move the iPhone Duo's hinge.\n");
            return 1;
        }
        static LeafHingeService *delegate;
        delegate = [LeafHingeService new];
        service = [objc_getClass("HIDVirtualEventService") new];
        ((void (*)(id, SEL, id))objc_msgSend)(service, sel_registerName("setDelegate:"), delegate);
        ((void (*)(id, SEL, id))objc_msgSend)(service, sel_registerName("setDispatchQueue:"), dispatch_get_main_queue());
        ((void (*)(id, SEL))objc_msgSend)(service, sel_registerName("activate"));
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0), ^{
            char line[64];
            while (fgets(line, sizeof line, stdin)) {
                double degrees = atof(line);
                dispatch_async(dispatch_get_main_queue(), ^{ send(degrees); });
            }
            // The pane closed: stop with it.
            dispatch_async(dispatch_get_main_queue(), ^{ exit(0); });
        });
        dispatch_main();
    }
}
