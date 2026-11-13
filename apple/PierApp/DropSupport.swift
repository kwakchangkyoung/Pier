import SwiftUI
import UniformTypeIdentifiers

/// 앱 내부 드래그(항목 이동)를 나타내는 pasteboard 타입.
/// NSTableView의 draggingSession:willBeginAtPoint에서 JSON 배열로 쓴다.
enum PierDragTypes {
    static let entryKeys = NSPasteboard.PasteboardType("com.yourname.pier.entry-keys")
    /// UTType으로도 등록해야 SwiftUI onDrop이 받을 수 있다.
    static let entryKeysType = UTType(exportedAs: entryKeys.rawValue, conformingTo: .data)
}

/// 드롭 페이로드. 외부 파일 URL(Finder→앱 업로드)과
/// 내부 항목 키(앱 안 이동)를 함께 담을 수 있다.
struct DropPayload {
    var urls: [URL] = []
    var keys: [String] = []

    var isEmpty: Bool { urls.isEmpty && keys.isEmpty }
}

/// SwiftUI `.onDrop` 델리게이트. breadcrumb 구간과 빈 폴더 영역에서 사용한다.
/// 테이블 위의 드롭은 NSTableView가 직접 처리하므로 여기까지 오지 않는다.
struct RemoteDropDelegate: DropDelegate {
    /// 드롭 대상 부모 폴더 키. 루트는 "".
    let parentKey: String
    /// 파싱이 끝난 페이로드를 넘기는 핸들러.
    let onDrop: @MainActor (DropPayload, String) -> Void

    static let types: [UTType] = [.fileURL, PierDragTypes.entryKeysType]

    func validate(info: DropInfo) -> Bool {
        info.hasItemsConforming(to: Self.types)
    }

    func performDrop(info: DropInfo) -> Bool {
        // 로딩은 비동기라 performDrop은 성공을 전제로 true를 반환하고,
        // 실제 페이로드 해석은 태스크에서 처리한다.
        Task { @MainActor in
            var payload = DropPayload()
            for provider in info.itemProviders(for: [UTType.fileURL]) {
                if let url = await Self.loadFileURL(provider) {
                    payload.urls.append(url)
                }
            }
            for provider in info.itemProviders(for: [PierDragTypes.entryKeysType]) {
                if let keys = await Self.loadKeys(provider) {
                    payload.keys.append(contentsOf: keys)
                }
            }
            if !payload.isEmpty {
                onDrop(payload, parentKey)
            }
        }
        return true
    }

    /// public.file-url 프로바이더에서 URL을 읽는다.
    private static func loadFileURL(_ item: NSItemProvider) async -> URL? {
        guard let value = try? await item.loadItem(forTypeIdentifier: UTType.fileURL.identifier)
        else { return nil }
        if let url = value as? URL { return url }
        if let url = value as? NSURL { return url as URL }
        if let data = value as? Data,
            let url = URL(dataRepresentation: data, relativeTo: nil)
        {
            return url
        }
        return nil
    }

    /// 내부 이동 마커(JSON 문자열 배열)를 읽는다.
    private static func loadKeys(_ item: NSItemProvider) async -> [String]? {
        guard
            let value = try? await item.loadItem(
                forTypeIdentifier: PierDragTypes.entryKeysType.identifier)
        else { return nil }
        let text: String?
        if let string = value as? String {
            text = string
        } else if let data = value as? Data {
            text = String(data: data, encoding: .utf8)
        } else {
            text = nil
        }
        guard let text, let data = text.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode([String].self, from: data)
    }
}
