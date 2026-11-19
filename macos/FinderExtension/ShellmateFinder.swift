import AppKit
import FinderSync

@objc(ShellmateFinderSync)
final class ShellmateFinderSync: FIFinderSync {
    private let menuIcon: NSImage? = {
        let image = Bundle(for: ShellmateFinderSync.self).image(forResource: "MenuIcon")
        image?.size = NSSize(width: 16, height: 16)
        image?.isTemplate = true
        return image
    }()

    override init() {
        super.init()
        let center = NSWorkspace.shared.notificationCenter
        for name in [NSWorkspace.didMountNotification, NSWorkspace.didUnmountNotification,
                     NSWorkspace.didRenameVolumeNotification] {
            center.addObserver(self, selector: #selector(volumesChanged(_:)), name: name, object: nil)
        }
        updateObservedDirectories()
    }

    @objc private func volumesChanged(_ notification: Notification) {
        updateObservedDirectories()
    }

    private func updateObservedDirectories() {
        var roots: Set<URL> = [URL(fileURLWithPath: "/", isDirectory: true),
                               URL(fileURLWithPath: "/Volumes", isDirectory: true)]
        // Include each mount point explicitly, including volumes mounted outside /Volumes.
        roots.formUnion(FileManager.default.mountedVolumeURLs(includingResourceValuesForKeys: nil) ?? [])
        FIFinderSyncController.default().directoryURLs = roots
    }

    override func menu(for menuKind: FIMenuKind) -> NSMenu? {
        guard menuKind == .contextualMenuForContainer || menuKind == .contextualMenuForItems else {
            return nil
        }
        let menu = NSMenu()
        let action = menuKind == .contextualMenuForItems ? #selector(openSelection(_:)) : #selector(openContainer(_:))
        let item = menu.addItem(withTitle: "进入 Shellmate", action: action, keyEquivalent: "")
        item.target = self
        item.image = menuIcon
        return menu
    }

    @objc private func openSelection(_ sender: Any?) {
        if let items = FIFinderSyncController.default().selectedItemURLs(), !items.isEmpty {
            openDirectories(items.lazy.filter(\.isFileURL).map { url in
                // Finder's directory flag also works when the sandbox limits metadata access.
                var isDirectory: ObjCBool = false
                if url.hasDirectoryPath { return url }
                let exists = FileManager.default.fileExists(atPath: url.path, isDirectory: &isDirectory)
                return exists && isDirectory.boolValue ? url : url.deletingLastPathComponent()
            })
        } else {
            openContainer(sender)
        }
    }

    @objc private func openContainer(_ sender: Any?) {
        if let target = FIFinderSyncController.default().targetedURL() {
            openDirectories([target])
        }
    }

    private func openDirectories(_ urls: some Sequence<URL>) {
        var seen = Set<String>()
        var paths = [URLQueryItem]()
        for url in urls where url.isFileURL {
            let path = url.resolvingSymlinksInPath().standardizedFileURL.path
            if seen.insert(path).inserted {
                paths.append(URLQueryItem(name: "path", value: path))
                if paths.count == 10 { break }
            }
        }
        guard !paths.isEmpty else { return }
        var components = URLComponents()
        components.scheme = "shellmate"
        components.host = "open"
        components.queryItems = paths
        // URLComponents leaves literal '+' in a query, which form-style parsers read as a space.
        components.percentEncodedQuery = components.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%2B")
        if let url = components.url { NSWorkspace.shared.open(url) }
    }
}
