import Foundation

/// 전송(업로드/다운로드) 항목 하나의 상태.
struct TransferItem: Identifiable, Equatable {
    enum Direction: Equatable {
        case download
        case upload
    }

    enum State: Equatable {
        case active
        case completed
        case failed(String)
    }

    let id = UUID()
    /// 표시 이름. 다운로드는 원격 이름, 업로드는 로컬 파일 이름.
    let name: String
    /// S3 객체 키. 파일은 객체 키, 폴더는 '/'로 끝나는 접두사.
    let key: String
    /// 폴더 전송 작업인지 여부.
    let isFolder: Bool
    let direction: Direction
    var state: State = .active
    var transferred: UInt64 = 0
    var total: UInt64 = 0
    /// 초당 바이트. progress 콜백이 올 때마다 갱신한다.
    var speed: Double = 0
    /// 다운로드: 완료 시 로컬 저장 경로. 업로드: 로컬 소스 경로.
    var destinationPath: String?
    /// 폴더 작업의 파일 수. 다운로드는 받은 수, 업로드는 올린 수.
    var fileCount: UInt64 = 0
    /// 폴더 업로드에서 제외 규칙으로 건너뛴 파일 수.
    var ignoredCount: UInt64 = 0
    /// 폴더 업로드에서 충돌로 건너뛴 파일 수.
    var skippedCount: UInt64 = 0
}
