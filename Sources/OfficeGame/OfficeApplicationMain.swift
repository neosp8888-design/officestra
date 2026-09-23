import AppKit
import SwiftUI

@main
enum OfficeApplicationMain {
    @MainActor
    static func main() {
        // SwiftUI creates its own AppKitApplication before NSApplicationMain
        // consults NSPrincipalClass. Establish our dispatcher first so hover
        // and wheel coalescing actually runs in the shipped app.
        _ = OfficeApplication.shared
        OfficeGameApp.main()
    }
}
