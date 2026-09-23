// A fresh process is essential: XCTest already has an NSApplication instance.
import AppKit
import SwiftUI

struct OfficeGameApp: App {
    var body: some Scene {
        WindowGroup {
            Text("Input dispatcher validation")
                .frame(width: 200, height: 60)
                .onAppear {
                    DispatchQueue.main.async {
                        let app = NSApplication.shared
                        let selector = #selector(NSApplication.sendEvent(_:))
                        let actual = class_getMethodImplementation(type(of: app), selector)
                        let expected = class_getMethodImplementation(OfficeApplication.self, selector)
                        let matches = actual != nil && actual == expected
                        print("DISPATCHER=\(NSStringFromClass(type(of: app)))")
                        print("CUSTOM_DISPATCHER=\(app is OfficeApplication)")
                        print("CUSTOM_SEND_EVENT=\(matches)")
                        fflush(stdout)
                        exit(app is OfficeApplication && matches ? 0 : 1)
                    }
                }
        }
    }
}
