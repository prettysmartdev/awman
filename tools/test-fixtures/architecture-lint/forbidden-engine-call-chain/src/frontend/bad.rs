fn notices(state: State) {
    let notices = state
        .handles
        .engines()
        .container_runtime
        .as_ref()
        .map(|r| r.take_notices());
}
