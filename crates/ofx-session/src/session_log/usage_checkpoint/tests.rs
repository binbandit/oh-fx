use super::*;

fn indeterminate() -> Result<(), SessionError> {
    Err(SessionError::Storage(DurableError::PostRenameFailed))
}

#[test]
fn a_newer_checkpoint_that_may_have_replaced_the_file_fences_older_snapshots() {
    let mut writer = CheckpointWriter::default();
    assert_eq!(writer.save(2, indeterminate), indeterminate());
    let mut older_written = false;
    assert_eq!(
        writer.save(1, || {
            older_written = true;
            Ok(())
        }),
        Ok(())
    );
    assert!(!older_written);
    assert_eq!(writer.written, 2);
}

#[test]
fn a_checkpoint_that_failed_before_its_rename_leaves_older_snapshots_free_to_save() {
    let mut writer = CheckpointWriter::default();
    let failed = Err(SessionError::Storage(DurableError::PreRenameFailed));
    assert_eq!(writer.save(2, || failed), failed);
    let mut older_written = false;
    assert_eq!(
        writer.save(1, || {
            older_written = true;
            Ok(())
        }),
        Ok(())
    );
    assert!(older_written);
    assert_eq!(writer.written, 1);
    assert_eq!(writer.save(3, || Ok(())), Ok(()));
    assert_eq!(writer.written, 3);
}
