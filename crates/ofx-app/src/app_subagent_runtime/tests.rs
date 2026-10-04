use super::*;

struct Catalog {
    notices: Mutex<Vec<String>>,
}

impl DynamicTools for Catalog {
    fn generation(&self) -> u64 {
        7
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut self.notices.lock().unwrap())
    }
}

#[test]
fn a_child_reads_the_parents_mcp_catalog_and_leaves_its_notices_to_the_parent() {
    let source = Arc::new(Catalog {
        notices: Mutex::new(vec!["[context] schema left out".to_owned()]),
    });
    let child = ParentCatalog::shared(Some(Arc::clone(&source) as Arc<dyn DynamicTools>))
        .expect("a catalog");
    assert_eq!(child.generation(), 7);
    assert!(child.tools().is_empty());
    assert!(child.take_notices().is_empty());
    assert_eq!(source.take_notices(), ["[context] schema left out"]);
    assert!(ParentCatalog::shared(None).is_none());
}
