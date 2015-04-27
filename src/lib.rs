#![crate_name = "polymap"]
#![feature(collections, core)]

use std::any::{Any, TypeId};
use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::hash_map;
use std::hash::Hash;
use std::intrinsics::needs_drop;
use std::mem::{align_of, size_of};
use std::ptr;

fn align(offset: usize, alignment: usize) -> usize {
    match offset % alignment {
        0 => offset,
        n => offset + (alignment - n),
    }
}

/// A key-value map that can contain varying types of values.
///
/// A single `PolyMap` instance can map a given key to varying types of values.
/// Successive operations on this key must use the correct type or the operation
/// will panic.
///
/// # Example
///
/// ```
/// use polymap::PolyMap;
///
/// let mut map = PolyMap::new();
///
/// // Maps `&str` to `&str`.
/// map.insert("foo", "Hello, world!");
///
/// // Maps `&str` to `i32`.
/// map.insert("bar", 123);
///
/// // Gets a reference to the stored member.
/// let foo: &&str = map.get("foo").unwrap();
/// println!("Got our string back: {}", foo);
///
/// let bar: &i32 = map.get("bar").unwrap();
/// println!("And here's our i32: {}", bar);
/// ```
///
/// # Notes
///
/// Values are stored in an internal buffer that is reallocated when exhausted.
/// Methods `reserve_data`, `reserve_data_exact`, and constructor `with_capacity`
/// can be used to reserve a larger buffer ahead of time to prevent expensive
/// reallocation and move operations.
///
#[derive(Default)]
pub struct PolyMap<K: Eq + Hash> {
    /// Value data store
    data: Vec<u8>,
    /// Maps key to field offset
    field_map: HashMap<K, usize>,
    /// Inserted fields, sorted by offset
    fields: Vec<Field>,
}

/// Private `PolyMap` field descriptor.
///
/// Contains the field size and offset, as well as `TypeId`,
/// which is used to identify a type for successive operations, and `drop`,
/// which is used to call a destructor ("drop glue") when `PolyMap::clear`
/// is called or a `PolyMap` instance goes out of scope.
struct Field {
    offset: usize,
    size: usize,
    id: TypeId,
    drop: Option<fn(*const ())>,
}

/// Drops the pointed-to value as `T`.
fn drop_ptr<T>(p: *const ()) {
    unsafe { ptr::read(p as *const T); }
}

impl<K: Eq + Hash> PolyMap<K> {
    /// Constructs a new `PolyMap`.
    pub fn new() -> PolyMap<K> {
        PolyMap{
            data: Vec::new(),
            field_map: HashMap::new(),
            fields: Vec::new(),
        }
    }

    /// Constructs a new `PolyMap` with space reserved for `n` fields
    /// and `size` bytes of data.
    pub fn with_capacity(n: usize, size: usize) -> PolyMap<K> {
        PolyMap{
            data: Vec::with_capacity(size),
            field_map: HashMap::with_capacity(n),
            fields: Vec::with_capacity(n),
        }
    }

    /// Removes all key-value pairs from the map, calling any destructors on
    /// stored values.
    pub fn clear(&mut self) {
        while let Some(f) = self.fields.pop() {
            if let Some(dropper) = f.drop {
                dropper(self.get_data::<()>(f.offset));
            }
        }
    }

    /// Returns whether the map contains a value corresponding to the given key.
    /// Does not make any assertions about the type of the value.
    pub fn contains_key<Q: ?Sized>(&self, k: &Q) -> bool
            where K: Borrow<Q>, Q: Eq + Hash {
        self.field_map.contains_key(k)
    }

    /// Returns whether the map contains a value corresponding to the given key
    /// whose type is the same as the given type.
    ///
    /// # Example
    ///
    /// ```
    /// use polymap::PolyMap;
    ///
    /// let mut map = PolyMap::new();
    ///
    /// map.insert("foo", 1);
    /// assert_eq!(false, map.contains_key_of::<_, &str>("foo"));
    /// assert_eq!(true, map.contains_key_of::<_, i32>("foo"));
    /// ```
    pub fn contains_key_of<Q: ?Sized, T: Any>(&self, k: &Q) -> bool
            where K: Borrow<Q>, Q: Eq + Hash {
        let id = TypeId::of::<T>();
        self.get_field(k).map(|f| f.id == id) == Some(true)
    }

    /// Returns the capacity, in bytes, of the internal data buffer.
    pub fn data_capacity(&self) -> usize {
        self.data.capacity()
    }

    /// Returns the size, in bytes, of the internal data buffer.
    pub fn data_size(&self) -> usize {
        self.data.len()
    }

    /// Returns a reference to the value corresponding to the given key.
    ///
    /// If the key is not contained within the map, `None` will be returned.
    ///
    /// # Panics
    ///
    /// If the key exists, but the type of value differs from the one requested.
    pub fn get<Q: ?Sized, T: Any>(&self, k: &Q) -> Option<&T>
            where K: Borrow<Q>, Q: Eq + Hash {
        self.get_field_with_id(k, TypeId::of::<T>())
            .map(|f| unsafe { &*self.get_data(f.offset) })
    }

    /// Returns a mutable reference to the value corresponding to the given key.
    ///
    /// If the key is not contained within the map, `None` will be returned.
    ///
    /// # Panics
    ///
    /// If the key exists, but the type of value differs from the one requested.
    pub fn get_mut<Q: ?Sized, T: Any>(&mut self, k: &Q) -> Option<&mut T>
            where K: Borrow<Q>, Q: Eq + Hash {
        self.get_field_with_id(k, TypeId::of::<T>())
            .map(|f| f.offset)
            .map(|offset| unsafe { &mut *self.get_data_mut(offset) })
    }

    /// Inserts a key-value pair into the map. If the key is already present,
    /// that value is returned. Otherwise, `None` is returned.
    ///
    /// # Panics
    ///
    /// If the key exists, but has a value of a different type than the one given.
    pub fn insert<T: Any>(&mut self, k: K, t: T) -> Option<T> {
        let offset = self.get_field(&k).map(|f| {
            if f.id != TypeId::of::<T>() {
                panic!("insert with value of different type");
            }
            f.offset
        });

        unsafe {
            if let Some(offset) = offset {
                Some(ptr::replace(self.get_data_mut(offset), t))
            } else {
                let offset = self.allocate::<T>(k);
                ptr::write(self.get_data_mut(offset), t);
                None
            }
        }
    }

    /// Returns an iterator visiting all keys in arbitrary order.
    /// Iterator element type is `&K`.
    pub fn keys(&self) -> Keys<K> {
        Keys{iter: self.field_map.keys()}
    }

    /// Returns the number of elements in the map.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Reserves capacity for at least `additional` additional bytes of storage
    /// space within the internal data buffer.
    pub fn reserve_data(&mut self, additional: usize) {
        self.data.reserve(additional);
    }

    /// Reserves space for at least `n` bytes in the internal data buffer.
    /// Does nothing if the capacity is already sufficient.
    pub fn reserve_data_exact(&mut self, n: usize) {
        self.data.reserve_exact(n);
    }

    /// Reserves capacity for at least `additional` additional fields.
    pub fn reserve_fields(&mut self, additional: usize) {
        self.fields.reserve(additional);
    }

    /// Reserves space for at least `n` fields.
    /// Does nothing if the capacity is already sufficient.
    pub fn reserve_fields_exact(&mut self, n: usize) {
        self.fields.reserve_exact(n);
    }

    /// Removes a key from the map, returning the value if one existed.
    ///
    /// # Panics
    ///
    /// If the key exists, but the type of value differs from the one requested.
    pub fn remove<Q: ?Sized, T: Any>(&mut self, k: &Q) -> Option<T>
            where K: Borrow<Q>, Q: Eq + Hash {
        let id = TypeId::of::<T>();

        let pos = self.get_offset(k).map(|offset|
            self.fields.binary_search_by(|f| f.offset.cmp(&offset)).unwrap());

        pos.map(|pos| {
            if self.fields[pos].id != id {
                panic!("remove value of a different type");
            }
            self.field_map.remove(k).unwrap();
            let f = self.fields.remove(pos);
            unsafe {
                let p = self.get_data(f.offset);
                ptr::read(p)
            }
        })
    }

    /// Shrinks the internal data buffer as close as possible to the size of
    /// the currently contained elements.
    pub fn shrink_data_to_fit(&mut self) {
        // TODO: Make an effort to condense elements within allocated space
        self.data.shrink_to_fit();
    }

    /// Allocates space for an object of given size and alignment.
    /// Grows buffer if necessary. Returns offset of new object.
    fn allocate<T: Any>(&mut self, k: K) -> usize {
        let id = TypeId::of::<T>();

        let (size, alignment) = match size_of::<T>() {
            0 => (1, 1),
            n => (n, align_of::<T>())
        };

        let (offset, index) = if self.fields.is_empty() ||
                size <= self.fields[0].offset {
            (0, 0)
        } else {
            let ia = self.fields.iter();
            let ib = self.fields.iter().skip(1);
            let mut res = None;

            for (i, (a, b)) in ia.zip(ib).enumerate() {
                let off = align(a.offset + a.size, alignment);

                if off + size <= b.offset {
                    res = Some((off, i + 1));
                    break;
                }
            }

            res.unwrap_or_else(|| {
                let last = self.fields.last().unwrap();
                (align(last.offset + last.size, alignment), self.fields.len())
            })
        };

        if self.data.len() < offset + size {
            self.data.resize(offset + size, 0u8);
        }

        self.field_map.insert(k, offset);
        self.fields.insert(index, Field{
            offset: offset,
            size: size,
            id: id,
            drop: if unsafe { needs_drop::<T>() } {
                Some(drop_ptr::<T>)
            } else {
                None
            },
        });

        offset
    }

    /// Returns a pointer to `T` at the given offset.
    /// Does not perform any bounds checking.
    fn get_data<T: Any>(&self, offset: usize) -> *const T {
        unsafe { self.data.as_ptr().offset(offset as isize) as *const T }
    }

    /// Returns a mutable pointer to `T` at the given offset.
    /// Does not perform any bounds checking.
    fn get_data_mut<T: Any>(&mut self, offset: usize) -> *mut T {
        unsafe { self.data.as_mut_ptr().offset(offset as isize) as *mut T }
    }

    /// Returns a reference to the field descriptor for the given key.
    fn get_field<Q: ?Sized>(&self, k: &Q) -> Option<&Field>
            where K: Borrow<Q>, Q: Eq + Hash {
        self.field_map.get(k).map(|off| {
            let pos = self.fields.binary_search_by(|f| f.offset.cmp(off)).unwrap();
            &self.fields[pos]
        })
    }

    fn get_offset<Q: ?Sized>(&self, k: &Q) -> Option<usize>
            where K: Borrow<Q>, Q: Eq + Hash {
        self.field_map.get(k).map(|&o| o)
    }

    /// Returns a reference to the field descriptor for the given key.
    ///
    /// # Panics
    ///
    /// If the given field has a different `TypeId` than the one given.
    fn get_field_with_id<Q: ?Sized>(&self, k: &Q, id: TypeId) -> Option<&Field>
            where K: Borrow<Q>, Q: Eq + Hash {
        self.get_field(k).map(|f| {
            if f.id != id {
                panic!("lookup for value of a different type");
            }
            f
        })
    }
}

impl<K: Eq + Hash> Drop for PolyMap<K> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Iterator over the keys of a `PolyMap`
#[derive(Clone)]
pub struct Keys<'a, K: 'a> {
    iter: hash_map::Keys<'a, K, usize>
}

impl<'a, K> Iterator for Keys<'a, K> {
    type Item = &'a K;

    fn next(&mut self) -> Option<&'a K> {
        self.iter.next()
    }
}

#[cfg(test)]
mod tests {
    use super::PolyMap;
    use std::sync::atomic::{AtomicUsize, ATOMIC_USIZE_INIT};
    use std::sync::atomic::Ordering::SeqCst;

    #[test]
    fn test_contains() {
        let mut map = PolyMap::new();

        map.insert("a", 1);
        assert!(map.contains_key("a"));
        assert!(!map.contains_key("b"));

        assert!(map.contains_key_of::<_, i32>("a"));
        assert!(!map.contains_key_of::<_, ()>("a"));
        assert!(!map.contains_key_of::<_, i32>("b"));
    }

    static DROP_COUNT: AtomicUsize = ATOMIC_USIZE_INIT;

    struct Dropper { n: usize }

    impl Drop for Dropper {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(self.n, SeqCst);
        }
    }

    #[test]
    fn test_drop() {
        DROP_COUNT.store(0, SeqCst);

        {
            let mut map = PolyMap::new();
            map.insert(0, Dropper{n: 1});
            map.insert(1, Dropper{n: 2});
            map.insert(2, Dropper{n: 3});
        }

        assert_eq!(DROP_COUNT.load(SeqCst), 6);
    }

    #[test]
    fn test_keys() {
        use std::collections::HashSet;

        let mut map = PolyMap::new();

        map.insert(0, 0xaa_u8);
        map.insert(1, 0xbb_u8);
        map.insert(2, 0xcc_u8);
        map.insert(3, 0xdd_u8);

        let keys: HashSet<u32> = map.keys().map(|i| *i).collect();
        assert_eq!(keys, vec![0, 1, 2, 3].into_iter().collect());
    }

    #[test]
    #[should_panic]
    fn test_mismatch_get() {
        let mut map = PolyMap::new();

        map.insert("a", 0xAAAAAAAA_u32);
        let _a: Option<&i32> = map.get("a");
    }

    #[test]
    #[should_panic]
    fn test_mismatch_insert() {
        let mut map = PolyMap::new();

        map.insert("a", 1i32);
        map.insert("a", 1u32);
    }

    #[test]
    #[should_panic]
    fn test_mismatch_remove() {
        let mut map = PolyMap::new();

        map.insert("a", 1);
        let _ = map.remove::<_, u32>("a");
    }

    #[test]
    fn test_packing() {
        let mut map = PolyMap::new();

        map.insert("a", 0xAA_u8);
        map.insert("b", 0xBBBB_u16);
        map.insert("c", 0xCC_u8);

        assert_eq!(map.get("a"), Some(&0xAA_u8));
        assert_eq!(map.get("b"), Some(&0xBBBB_u16));
        assert_eq!(map.get("c"), Some(&0xCC_u8));

        assert_eq!(map.data_size(), 4);

        let mut map = PolyMap::new();

        map.insert("a", 0xAAAA_u16);
        map.insert("b", 0xBBBBBBBB_u32);
        map.insert("c", 0xCC_u8);
        map.insert("d", 0xDD_u8);

        assert_eq!(map.get("a"), Some(&0xAAAA_u16));
        assert_eq!(map.get("b"), Some(&0xBBBBBBBB_u32));
        assert_eq!(map.get("c"), Some(&0xCC_u8));
        assert_eq!(map.get("d"), Some(&0xDD_u8));

        assert_eq!(map.data_size(), 8);
    }

    #[test]
    fn test_remove() {
        let mut map = PolyMap::new();

        map.insert("a", 0x87654321_u32);
        assert_eq!(map.remove("a"), Some(0x87654321_u32));
        assert_eq!(map.get::<_, u32>("a"), None);

        let b = "foo".to_string();
        map.insert("b", b);
        assert_eq!(map.get("b"), Some(&"foo".to_string()));

        let bb: String = map.remove("b").unwrap();
        assert_eq!(bb, "foo");
    }

    #[test]
    fn test_replace() {
        let mut map = PolyMap::new();

        map.insert("a", 0xAAAAAAAA_u32);
        assert_eq!(map.insert("a", 0xBBBBBBBB_u32), Some(0xAAAAAAAA_u32));
        assert_eq!(map.get("a"), Some(&0xBBBBBBBB_u32));

        map.insert("b", 0xCCCCCCCC_u32);
        assert_eq!(map.remove("b"), Some(0xCCCCCCCC_u32));
        assert_eq!(map.insert("c", 0xDDDDDDDDDDDDDDDD_u64), None);
    }

    #[test]
    fn test_reuse() {
        let mut map = PolyMap::new();

        map.insert("a", 0xAAAAAAAA_u32);
        map.insert("b", 0xBBBBBBBB_u32);

        assert_eq!(map.get("b"), Some(&0xBBBBBBBB_u32));
        assert_eq!(map.remove("a"), Some(0xAAAAAAAA_u32));

        map.insert("c", 0xCCCCCCCC_u32);

        assert_eq!(map.get("c"), Some(&0xCCCCCCCC_u32));
        assert_eq!(map.data_size(), 8);
    }

    #[test]
    fn test_insert() {
        let mut map = PolyMap::new();

        assert_eq!(map.insert("a", 0x12345678_u32), None);
        assert_eq!(map.insert("b", 0x12345678_u32), None);
        assert_eq!(map.get("a"), Some(&0x12345678_u32));
        assert_eq!(map.get("b"), Some(&0x12345678_u32));
        assert_eq!(map.get("c"), None::<&u32>);
    }

    #[test]
    fn test_strings() {
        let mut map = PolyMap::new();

        map.insert("a".to_string(), "a".to_string());
        map.insert("b".to_string(), "b".to_string());

        assert_eq!(map.get::<_, String>("a"), Some(&"a".to_string()));
        assert_eq!(map.get::<_, String>("b"), Some(&"b".to_string()));
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    struct A;

    #[test]
    fn test_zero_size() {
        let mut map = PolyMap::new();

        map.insert("a", A);
        map.insert("b", A);
        map.insert("c", A);

        assert_eq!(map.get("a"), Some(&A));
        assert_eq!(map.get("b"), Some(&A));
        assert_eq!(map.get("c"), Some(&A));
        assert_eq!(map.data_size(), 3);

        let aptr = map.get::<_, A>("a").unwrap() as *const A;
        let bptr = map.get::<_, A>("b").unwrap() as *const A;
        let cptr = map.get::<_, A>("c").unwrap() as *const A;

        assert!(aptr != bptr && bptr != cptr);
    }
}
