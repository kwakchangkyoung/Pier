import AppKit

/// 파일 테이블 전용 NSTableView. Return(이름 변경)과 ⌘⌫(삭제) 키를
/// 첫 응답자일 때 가로채 코디네이터로 전달한다.
/// 텍스트 필드 등 다른 응답자에게는 그대로 넘어간다.
final class BrowserTableView: NSTableView {
    /// Return 키. 선택이 하나일 때만 호출된다.
    var onReturnKey: (() -> Void)?
    /// ⌘⌫ 키. 선택이 있을 때만 호출된다.
    var onDeleteKey: (() -> Void)?

    override func keyDown(with event: NSEvent) {
        let hasCommand = event.modifierFlags.contains(.command)
        switch event.keyCode {
        case 36, 76:  // Return / 키패드 Enter
            if selectedRowIndexes.count == 1 {
                onReturnKey?()
                return
            }
        case 51:  // Backspace(⌫)
            if hasCommand, !selectedRowIndexes.isEmpty {
                onDeleteKey?()
                return
            }
        default:
            break
        }
        super.keyDown(with: event)
    }
}
