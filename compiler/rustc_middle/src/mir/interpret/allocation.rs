//! The virtual memory representation of the MIR interpreter.

use std::borrow::Cow;
use std::convert::{TryFrom, TryInto};
use std::fmt;
use std::hash;
use std::iter;
use std::ops::{Deref, Range};
use std::ptr;

use rustc_ast::Mutability;
use rustc_data_structures::intern::Interned;
use rustc_data_structures::sorted_map::SortedMap;
use rustc_span::DUMMY_SP;
use rustc_target::abi::{Align, HasDataLayout, Size};

use super::{
    read_target_uint, write_target_uint, AllocId, InterpError, InterpResult, Pointer, Provenance,
    ResourceExhaustionInfo, Scalar, ScalarMaybeUninit, ScalarSizeMismatch, UndefinedBehaviorInfo,
    UninitBytesAccess, UnsupportedOpInfo,
};
use crate::ty;

/// This type represents an Allocation in the Miri/CTFE core engine.
///
/// Its public API is rather low-level, working directly with allocation offsets and a custom error
/// type to account for the lack of an AllocId on this level. The Miri/CTFE core engine `memory`
/// module provides higher-level access.
// Note: for performance reasons when interning, some of the `Allocation` fields can be partially
// hashed. (see the `Hash` impl below for more details), so the impl is not derived.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, TyEncodable, TyDecodable)]
#[derive(HashStable)]
pub struct Allocation<Tag = AllocId, Extra = ()> {
    /// The actual bytes of the allocation.
    /// Note that the bytes of a pointer represent the offset of the pointer.
    bytes: Box<[u8]>,
    /// Maps from byte addresses to extra data for each pointer.
    /// Only the first byte of a pointer is inserted into the map; i.e.,
    /// every entry in this map applies to `pointer_size` consecutive bytes starting
    /// at the given offset.
    relocations: Relocations<Tag>,
    /// Denotes which part of this allocation is initialized.
    init_mask: InitMask,
    /// The alignment of the allocation to detect unaligned reads.
    /// (`Align` guarantees that this is a power of two.)
    pub align: Align,
    /// `true` if the allocation is mutable.
    /// Also used by codegen to determine if a static should be put into mutable memory,
    /// which happens for `static mut` and `static` with interior mutability.
    pub mutability: Mutability,
    /// Extra state for the machine.
    pub extra: Extra,
}

/// This is the maximum size we will hash at a time, when interning an `Allocation` and its
/// `InitMask`. Note, we hash that amount of bytes twice: at the start, and at the end of a buffer.
/// Used when these two structures are large: we only partially hash the larger fields in that
/// situation. See the comment at the top of their respective `Hash` impl for more details.
const MAX_BYTES_TO_HASH: usize = 64;

/// This is the maximum size (in bytes) for which a buffer will be fully hashed, when interning.
/// Otherwise, it will be partially hashed in 2 slices, requiring at least 2 `MAX_BYTES_TO_HASH`
/// bytes.
const MAX_HASHED_BUFFER_LEN: usize = 2 * MAX_BYTES_TO_HASH;

// Const allocations are only hashed for interning. However, they can be large, making the hashing
// expensive especially since it uses `FxHash`: it's better suited to short keys, not potentially
// big buffers like the actual bytes of allocation. We can partially hash some fields when they're
// large.
impl hash::Hash for Allocation {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        // Partially hash the `bytes` buffer when it is large. To limit collisions with common
        // prefixes and suffixes, we hash the length and some slices of the buffer.
        let byte_count = self.bytes.len();
        if byte_count > MAX_HASHED_BUFFER_LEN {
            // Hash the buffer's length.
            byte_count.hash(state);

            // And its head and tail.
            self.bytes[..MAX_BYTES_TO_HASH].hash(state);
            self.bytes[byte_count - MAX_BYTES_TO_HASH..].hash(state);
        } else {
            self.bytes.hash(state);
        }

        // Hash the other fields as usual.
        self.relocations.hash(state);
        self.init_mask.hash(state);
        self.align.hash(state);
        self.mutability.hash(state);
        self.extra.hash(state);
    }
}

/// Interned types generally have an `Outer` type and an `Inner` type, where
/// `Outer` is a newtype around `Interned<Inner>`, and all the operations are
/// done on `Outer`, because all occurrences are interned. E.g. `Ty` is an
/// outer type and `TyS` is its inner type.
///
/// Here things are different because only const allocations are interned. This
/// means that both the inner type (`Allocation`) and the outer type
/// (`ConstAllocation`) are used quite a bit.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, HashStable)]
#[rustc_pass_by_value]
pub struct ConstAllocation<'tcx, Tag = AllocId, Extra = ()>(
    pub Interned<'tcx, Allocation<Tag, Extra>>,
);

impl<'tcx> fmt::Debug for ConstAllocation<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // This matches how `Allocation` is printed. We print it like this to
        // avoid having to update expected output in a lot of tests.
        write!(f, "{:?}", self.inner())
    }
}

impl<'tcx, Tag, Extra> ConstAllocation<'tcx, Tag, Extra> {
    pub fn inner(self) -> &'tcx Allocation<Tag, Extra> {
        self.0.0
    }
}

/// We have our own error type that does not know about the `AllocId`; that information
/// is added when converting to `InterpError`.
#[derive(Debug)]
pub enum AllocError {
    /// A scalar had the wrong size.
    ScalarSizeMismatch(ScalarSizeMismatch),
    /// Encountered a pointer where we needed raw bytes.
    ReadPointerAsBytes,
    /// Partially overwriting a pointer.
    PartialPointerOverwrite(Size),
    /// Using uninitialized data where it is not allowed.
    InvalidUninitBytes(Option<UninitBytesAccess>),
}
pub type AllocResult<T = ()> = Result<T, AllocError>;

impl From<ScalarSizeMismatch> for AllocError {
    fn from(s: ScalarSizeMismatch) -> Self {
        AllocError::ScalarSizeMismatch(s)
    }
}

impl AllocError {
    pub fn to_interp_error<'tcx>(self, alloc_id: AllocId) -> InterpError<'tcx> {
        use AllocError::*;
        match self {
            ScalarSizeMismatch(s) => {
                InterpError::UndefinedBehavior(UndefinedBehaviorInfo::ScalarSizeMismatch(s))
            }
            ReadPointerAsBytes => InterpError::Unsupported(UnsupportedOpInfo::ReadPointerAsBytes),
            PartialPointerOverwrite(offset) => InterpError::Unsupported(
                UnsupportedOpInfo::PartialPointerOverwrite(Pointer::new(alloc_id, offset)),
            ),
            InvalidUninitBytes(info) => InterpError::UndefinedBehavior(
                UndefinedBehaviorInfo::InvalidUninitBytes(info.map(|b| (alloc_id, b))),
            ),
        }
    }
}

/// The information that makes up a memory access: offset and size.
#[derive(Copy, Clone, Debug)]
pub struct AllocRange {
    pub start: Size,
    pub size: Size,
}

/// Free-starting constructor for less syntactic overhead.
#[inline(always)]
pub fn alloc_range(start: Size, size: Size) -> AllocRange {
    AllocRange { start, size }
}

impl AllocRange {
    #[inline(always)]
    pub fn end(self) -> Size {
        self.start + self.size // This does overflow checking.
    }

    /// Returns the `subrange` within this range; panics if it is not a subrange.
    #[inline]
    pub fn subrange(self, subrange: AllocRange) -> AllocRange {
        let sub_start = self.start + subrange.start;
        let range = alloc_range(sub_start, subrange.size);
        assert!(range.end() <= self.end(), "access outside the bounds for given AllocRange");
        range
    }
}

// The constructors are all without extra; the extra gets added by a machine hook later.
impl<Tag> Allocation<Tag> {
    /// Creates an allocation initialized by the given bytes
    pub fn from_bytes<'a>(
        slice: impl Into<Cow<'a, [u8]>>,
        align: Align,
        mutability: Mutability,
    ) -> Self {
        let bytes = Box::<[u8]>::from(slice.into());
        let size = Size::from_bytes(bytes.len());
        Self {
            bytes,
            relocations: Relocations::new(),
            init_mask: InitMask::new(size, true),
            align,
            mutability,
            extra: (),
        }
    }

    pub fn from_bytes_byte_aligned_immutable<'a>(slice: impl Into<Cow<'a, [u8]>>) -> Self {
        Allocation::from_bytes(slice, Align::ONE, Mutability::Not)
    }

    /// Try to create an Allocation of `size` bytes, failing if there is not enough memory
    /// available to the compiler to do so.
    pub fn uninit<'tcx>(size: Size, align: Align, panic_on_fail: bool) -> InterpResult<'tcx, Self> {
        let bytes = Box::<[u8]>::try_new_zeroed_slice(size.bytes_usize()).map_err(|_| {
            // This results in an error that can happen non-deterministically, since the memory
            // available to the compiler can change between runs. Normally queries are always
            // deterministic. However, we can be non-deterministic here because all uses of const
            // evaluation (including ConstProp!) will make compilation fail (via hard error
            // or ICE) upon encountering a `MemoryExhausted` error.
            if panic_on_fail {
                panic!("Allocation::uninit called with panic_on_fail had allocation failure")
            }
            ty::tls::with(|tcx| {
                tcx.sess.delay_span_bug(DUMMY_SP, "exhausted memory during interpretation")
            });
            InterpError::ResourceExhaustion(ResourceExhaustionInfo::MemoryExhausted)
        })?;
        // SAFETY: the box was zero-allocated, which is a valid initial value for Box<[u8]>
        let bytes = unsafe { bytes.assume_init() };
        Ok(Allocation {
            bytes,
            relocations: Relocations::new(),
            init_mask: InitMask::new(size, false),
            align,
            mutability: Mutability::Mut,
            extra: (),
        })
    }
}

impl Allocation {
    /// Convert Tag and add Extra fields
    pub fn convert_tag_add_extra<Tag, Extra, Err>(
        self,
        cx: &impl HasDataLayout,
        extra: Extra,
        mut tagger: impl FnMut(Pointer<AllocId>) -> Result<Pointer<Tag>, Err>,
    ) -> Result<Allocation<Tag, Extra>, Err> {
        // Compute new pointer tags, which also adjusts the bytes.
        let mut bytes = self.bytes;
        let mut new_relocations = Vec::with_capacity(self.relocations.0.len());
        let ptr_size = cx.data_layout().pointer_size.bytes_usize();
        let endian = cx.data_layout().endian;
        for &(offset, alloc_id) in self.relocations.iter() {
            let idx = offset.bytes_usize();
            let ptr_bytes = &mut bytes[idx..idx + ptr_size];
            let bits = read_target_uint(endian, ptr_bytes).unwrap();
            let (ptr_tag, ptr_offset) =
                tagger(Pointer::new(alloc_id, Size::from_bytes(bits)))?.into_parts();
            write_target_uint(endian, ptr_bytes, ptr_offset.bytes().into()).unwrap();
            new_relocations.push((offset, ptr_tag));
        }
        // Create allocation.
        Ok(Allocation {
            bytes,
            relocations: Relocations::from_presorted(new_relocations),
            init_mask: self.init_mask,
            align: self.align,
            mutability: self.mutability,
            extra,
        })
    }
}

/// Raw accessors. Provide access to otherwise private bytes.
impl<Tag, Extra> Allocation<Tag, Extra> {
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn size(&self) -> Size {
        Size::from_bytes(self.len())
    }

    /// Looks at a slice which may describe uninitialized bytes or describe a relocation. This differs
    /// from `get_bytes_with_uninit_and_ptr` in that it does no relocation checks (even on the
    /// edges) at all.
    /// This must not be used for reads affecting the interpreter execution.
    pub fn inspect_with_uninit_and_ptr_outside_interpreter(&self, range: Range<usize>) -> &[u8] {
        &self.bytes[range]
    }

    /// Returns the mask indicating which bytes are initialized.
    pub fn init_mask(&self) -> &InitMask {
        &self.init_mask
    }

    /// Returns the relocation list.
    pub fn relocations(&self) -> &Relocations<Tag> {
        &self.relocations
    }
}

/// Byte accessors.
impl<Tag: Provenance, Extra> Allocation<Tag, Extra> {
    /// This is the entirely abstraction-violating way to just grab the raw bytes without
    /// caring about relocations. It just deduplicates some code between `read_scalar`
    /// and `get_bytes_internal`.
    fn get_bytes_even_more_internal(&self, range: AllocRange) -> &[u8] {
        &self.bytes[range.start.bytes_usize()..range.end().bytes_usize()]
    }

    /// The last argument controls whether we error out when there are uninitialized or pointer
    /// bytes. However, we *always* error when there are relocations overlapping the edges of the
    /// range.
    ///
    /// You should never call this, call `get_bytes` or `get_bytes_with_uninit_and_ptr` instead,
    ///
    /// This function also guarantees that the resulting pointer will remain stable
    /// even when new allocations are pushed to the `HashMap`. `mem_copy_repeatedly` relies
    /// on that.
    ///
    /// It is the caller's responsibility to check bounds and alignment beforehand.
    fn get_bytes_internal(
        &self,
        cx: &impl HasDataLayout,
        range: AllocRange,
        check_init_and_ptr: bool,
    ) -> AllocResult<&[u8]> {
        if check_init_and_ptr {
            self.check_init(range)?;
            self.check_relocations(cx, range)?;
        } else {
            // We still don't want relocations on the *edges*.
            self.check_relocation_edges(cx, range)?;
        }

        Ok(self.get_bytes_even_more_internal(range))
    }

    /// Checks that these bytes are initialized and not pointer bytes, and then return them
    /// as a slice.
    ///
    /// It is the caller's responsibility to check bounds and alignment beforehand.
    /// Most likely, you want to use the `PlaceTy` and `OperandTy`-based methods
    /// on `InterpCx` instead.
    #[inline]
    pub fn get_bytes(&self, cx: &impl HasDataLayout, range: AllocRange) -> AllocResult<&[u8]> {
        self.get_bytes_internal(cx, range, true)
    }

    /// It is the caller's responsibility to handle uninitialized and pointer bytes.
    /// However, this still checks that there are no relocations on the *edges*.
    ///
    /// It is the caller's responsibility to check bounds and alignment beforehand.
    #[inline]
    pub fn get_bytes_with_uninit_and_ptr(
        &self,
        cx: &impl HasDataLayout,
        range: AllocRange,
    ) -> AllocResult<&[u8]> {
        self.get_bytes_internal(cx, range, false)
    }

    /// Just calling this already marks everything as defined and removes relocations,
    /// so be sure to actually put data there!
    ///
    /// It is the caller's responsibility to check bounds and alignment beforehand.
    /// Most likely, you want to use the `PlaceTy` and `OperandTy`-based methods
    /// on `InterpCx` instead.
    pub fn get_bytes_mut(
        &mut self,
        cx: &impl HasDataLayout,
        range: AllocRange,
    ) -> AllocResult<&mut [u8]> {
        self.mark_init(range, true);
        self.clear_relocations(cx, range)?;

        Ok(&mut self.bytes[range.start.bytes_usize()..range.end().bytes_usize()])
    }

    /// A raw pointer variant of `get_bytes_mut` that avoids invalidating existing aliases into this memory.
    pub fn get_bytes_mut_ptr(
        &mut self,
        cx: &impl HasDataLayout,
        range: AllocRange,
    ) -> AllocResult<*mut [u8]> {
        self.mark_init(range, true);
        self.clear_relocations(cx, range)?;

        assert!(range.end().bytes_usize() <= self.bytes.len()); // need to do our own bounds-check
        let begin_ptr = self.bytes.as_mut_ptr().wrapping_add(range.start.bytes_usize());
        let len = range.end().bytes_usize() - range.start.bytes_usize();
        Ok(ptr::slice_from_raw_parts_mut(begin_ptr, len))
    }
}

/// Reading and writing.
impl<Tag: Provenance, Extra> Allocation<Tag, Extra> {
    /// Validates that `ptr.offset` and `ptr.offset + size` do not point to the middle of a
    /// relocation. If `allow_uninit`/`allow_ptr` is `false`, also enforces that the memory in the
    /// given range contains no uninitialized bytes/relocations.
    pub fn check_bytes(
        &self,
        cx: &impl HasDataLayout,
        range: AllocRange,
        allow_uninit: bool,
        allow_ptr: bool,
    ) -> AllocResult {
        // Check bounds and relocations on the edges.
        self.get_bytes_with_uninit_and_ptr(cx, range)?;
        // Check uninit and ptr.
        if !allow_uninit {
            self.check_init(range)?;
        }
        if !allow_ptr {
            self.check_relocations(cx, range)?;
        }
        Ok(())
    }

    /// Reads a *non-ZST* scalar.
    ///
    /// If `read_provenance` is `true`, this will also read provenance; otherwise (if the machine
    /// supports that) provenance is entirely ignored.
    ///
    /// ZSTs can't be read because in order to obtain a `Pointer`, we need to check
    /// for ZSTness anyway due to integer pointers being valid for ZSTs.
    ///
    /// It is the caller's responsibility to check bounds and alignment beforehand.
    /// Most likely, you want to call `InterpCx::read_scalar` instead of this method.
    pub fn read_scalar(
        &self,
        cx: &impl HasDataLayout,
        range: AllocRange,
        read_provenance: bool,
    ) -> AllocResult<ScalarMaybeUninit<Tag>> {
        if read_provenance {
            assert_eq!(range.size, cx.data_layout().pointer_size);
        }

        // First and foremost, if anything is uninit, bail.
        if self.is_init(range).is_err() {
            // This inflates uninitialized bytes to the entire scalar, even if only a few
            // bytes are uninitialized.
            return Ok(ScalarMaybeUninit::Uninit);
        }

        // If we are doing a pointer read, and there is a relocation exactly where we
        // are reading, then we can put data and relocation back together and return that.
        if read_provenance && let Some(&prov) = self.relocations.get(&range.start) {
            // We already checked init and relocations, so we can use this function.
            let bytes = self.get_bytes_even_more_internal(range);
            let bits = read_target_uint(cx.data_layout().endian, bytes).unwrap();
            let ptr = Pointer::new(prov, Size::from_bytes(bits));
            return Ok(ScalarMaybeUninit::from_pointer(ptr, cx));
        }

        // If we are *not* reading a pointer, and we can just ignore relocations,
        // then do exactly that.
        if !read_provenance && Tag::OFFSET_IS_ADDR {
            // We just strip provenance.
            let bytes = self.get_bytes_even_more_internal(range);
            let bits = read_target_uint(cx.data_layout().endian, bytes).unwrap();
            return Ok(ScalarMaybeUninit::Scalar(Scalar::from_uint(bits, range.size)));
        }

        // It's complicated. Better make sure there is no provenance anywhere.
        // FIXME: If !OFFSET_IS_ADDR, this is the best we can do. But if OFFSET_IS_ADDR, then
        // `read_pointer` is true and we ideally would distinguish the following two cases:
        // - The entire `range` is covered by 2 relocations for the same provenance.
        //   Then we should return a pointer with that provenance.
        // - The range has inhomogeneous provenance. Then we should return just the
        //   underlying bits.
        let bytes = self.get_bytes(cx, range)?;
        let bits = read_target_uint(cx.data_layout().endian, bytes).unwrap();
        Ok(ScalarMaybeUninit::Scalar(Scalar::from_uint(bits, range.size)))
    }

    /// Writes a *non-ZST* scalar.
    ///
    /// ZSTs can't be read because in order to obtain a `Pointer`, we need to check
    /// for ZSTness anyway due to integer pointers being valid for ZSTs.
    ///
    /// It is the caller's responsibility to check bounds and alignment beforehand.
    /// Most likely, you want to call `InterpCx::write_scalar` instead of this method.
    #[instrument(skip(self, cx), level = "debug")]
    pub fn write_scalar(
        &mut self,
        cx: &impl HasDataLayout,
        range: AllocRange,
        val: ScalarMaybeUninit<Tag>,
    ) -> AllocResult {
        assert!(self.mutability == Mutability::Mut);

        let val = match val {
            ScalarMaybeUninit::Scalar(scalar) => scalar,
            ScalarMaybeUninit::Uninit => {
                return self.write_uninit(cx, range);
            }
        };

        // `to_bits_or_ptr_internal` is the right method because we just want to store this data
        // as-is into memory.
        let (bytes, provenance) = match val.to_bits_or_ptr_internal(range.size)? {
            Err(val) => {
                let (provenance, offset) = val.into_parts();
                (u128::from(offset.bytes()), Some(provenance))
            }
            Ok(data) => (data, None),
        };

        let endian = cx.data_layout().endian;
        let dst = self.get_bytes_mut(cx, range)?;
        write_target_uint(endian, dst, bytes).unwrap();

        // See if we have to also write a relocation.
        if let Some(provenance) = provenance {
            self.relocations.0.insert(range.start, provenance);
        }

        Ok(())
    }

    /// Write "uninit" to the given memory range.
    pub fn write_uninit(&mut self, cx: &impl HasDataLayout, range: AllocRange) -> AllocResult {
        self.mark_init(range, false);
        self.clear_relocations(cx, range)?;
        return Ok(());
    }
}

/// Relocations.
impl<Tag: Copy, Extra> Allocation<Tag, Extra> {
    /// Returns all relocations overlapping with the given pointer-offset pair.
    fn get_relocations(&self, cx: &impl HasDataLayout, range: AllocRange) -> &[(Size, Tag)] {
        // We have to go back `pointer_size - 1` bytes, as that one would still overlap with
        // the beginning of this range.
        let start = range.start.bytes().saturating_sub(cx.data_layout().pointer_size.bytes() - 1);
        self.relocations.range(Size::from_bytes(start)..range.end())
    }

    /// Returns whether this allocation has relocations overlapping with the given range.
    ///
    /// Note: this function exists to allow `get_relocations` to be private, in order to somewhat
    /// limit access to relocations outside of the `Allocation` abstraction.
    ///
    pub fn has_relocations(&self, cx: &impl HasDataLayout, range: AllocRange) -> bool {
        !self.get_relocations(cx, range).is_empty()
    }

    /// Checks that there are no relocations overlapping with the given range.
    #[inline(always)]
    fn check_relocations(&self, cx: &impl HasDataLayout, range: AllocRange) -> AllocResult {
        if self.has_relocations(cx, range) { Err(AllocError::ReadPointerAsBytes) } else { Ok(()) }
    }

    /// Removes all relocations inside the given range.
    /// If there are relocations overlapping with the edges, they
    /// are removed as well *and* the bytes they cover are marked as
    /// uninitialized. This is a somewhat odd "spooky action at a distance",
    /// but it allows strictly more code to run than if we would just error
    /// immediately in that case.
    fn clear_relocations(&mut self, cx: &impl HasDataLayout, range: AllocRange) -> AllocResult
    where
        Tag: Provenance,
    {
        // Find the start and end of the given range and its outermost relocations.
        let (first, last) = {
            // Find all relocations overlapping the given range.
            let relocations = self.get_relocations(cx, range);
            if relocations.is_empty() {
                return Ok(());
            }

            (
                relocations.first().unwrap().0,
                relocations.last().unwrap().0 + cx.data_layout().pointer_size,
            )
        };
        let start = range.start;
        let end = range.end();

        // We need to handle clearing the relocations from parts of a pointer.
        // FIXME: Miri should preserve partial relocations; see
        // https://github.com/rust-lang/miri/issues/2181.
        if first < start {
            if Tag::ERR_ON_PARTIAL_PTR_OVERWRITE {
                return Err(AllocError::PartialPointerOverwrite(first));
            }
            warn!(
                "Partial pointer overwrite! De-initializing memory at offsets {first:?}..{start:?}."
            );
            self.init_mask.set_range(first, start, false);
        }
        if last > end {
            if Tag::ERR_ON_PARTIAL_PTR_OVERWRITE {
                return Err(AllocError::PartialPointerOverwrite(
                    last - cx.data_layout().pointer_size,
                ));
            }
            warn!(
                "Partial pointer overwrite! De-initializing memory at offsets {end:?}..{last:?}."
            );
            self.init_mask.set_range(end, last, false);
        }

        // Forget all the relocations.
        // Since relocations do not overlap, we know that removing until `last` (exclusive) is fine,
        // i.e., this will not remove any other relocations just after the ones we care about.
        self.relocations.0.remove_range(first..last);

        Ok(())
    }

    /// Errors if there are relocations overlapping with the edges of the
    /// given memory range.
    #[inline]
    fn check_relocation_edges(&self, cx: &impl HasDataLayout, range: AllocRange) -> AllocResult {
        self.check_relocations(cx, alloc_range(range.start, Size::ZERO))?;
        self.check_relocations(cx, alloc_range(range.end(), Size::ZERO))?;
        Ok(())
    }
}

/// "Relocations" stores the provenance information of pointers stored in memory.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, TyEncodable, TyDecodable)]
pub struct Relocations<Tag = AllocId>(SortedMap<Size, Tag>);

impl<Tag> Relocations<Tag> {
    pub fn new() -> Self {
        Relocations(SortedMap::new())
    }

    // The caller must guarantee that the given relocations are already sorted
    // by address and contain no duplicates.
    pub fn from_presorted(r: Vec<(Size, Tag)>) -> Self {
        Relocations(SortedMap::from_presorted_elements(r))
    }
}

impl<Tag> Deref for Relocations<Tag> {
    type Target = SortedMap<Size, Tag>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A partial, owned list of relocations to transfer into another allocation.
///
/// Offsets are already adjusted to the destination allocation.
pub struct AllocationRelocations<Tag> {
    dest_relocations: Vec<(Size, Tag)>,
}

impl<Tag: Copy, Extra> Allocation<Tag, Extra> {
    pub fn prepare_relocation_copy(
        &self,
        cx: &impl HasDataLayout,
        src: AllocRange,
        dest: Size,
        count: u64,
    ) -> AllocationRelocations<Tag> {
        let relocations = self.get_relocations(cx, src);
        if relocations.is_empty() {
            return AllocationRelocations { dest_relocations: Vec::new() };
        }

        let size = src.size;
        let mut new_relocations = Vec::with_capacity(relocations.len() * (count as usize));

        // If `count` is large, this is rather wasteful -- we are allocating a big array here, which
        // is mostly filled with redundant information since it's just N copies of the same `Tag`s
        // at slightly adjusted offsets. The reason we do this is so that in `mark_relocation_range`
        // we can use `insert_presorted`. That wouldn't work with an `Iterator` that just produces
        // the right sequence of relocations for all N copies.
        for i in 0..count {
            new_relocations.extend(relocations.iter().map(|&(offset, reloc)| {
                // compute offset for current repetition
                let dest_offset = dest + size * i; // `Size` operations
                (
                    // shift offsets from source allocation to destination allocation
                    (offset + dest_offset) - src.start, // `Size` operations
                    reloc,
                )
            }));
        }

        AllocationRelocations { dest_relocations: new_relocations }
    }

    /// Applies a relocation copy.
    /// The affected range, as defined in the parameters to `prepare_relocation_copy` is expected
    /// to be clear of relocations.
    ///
    /// This is dangerous to use as it can violate internal `Allocation` invariants!
    /// It only exists to support an efficient implementation of `mem_copy_repeatedly`.
    pub fn mark_relocation_range(&mut self, relocations: AllocationRelocations<Tag>) {
        self.relocations.0.insert_presorted(relocations.dest_relocations);
    }
}

////////////////////////////////////////////////////////////////////////////////
// Uninitialized byte tracking
////////////////////////////////////////////////////////////////////////////////

type Block = u64;

/// A bitmask where each bit refers to the byte with the same index. If the bit is `true`, the byte
/// is initialized. If it is `false` the byte is uninitialized.
// Note: for performance reasons when interning, some of the `InitMask` fields can be partially
// hashed. (see the `Hash` impl below for more details), so the impl is not derived.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, TyEncodable, TyDecodable)]
#[derive(HashStable)]
pub struct InitMask {
    blocks: Vec<Block>,
    len: Size,
}

// Const allocations are only hashed for interning. However, they can be large, making the hashing
// expensive especially since it uses `FxHash`: it's better suited to short keys, not potentially
// big buffers like the allocation's init mask. We can partially hash some fields when they're
// large.
impl hash::Hash for InitMask {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        const MAX_BLOCKS_TO_HASH: usize = MAX_BYTES_TO_HASH / std::mem::size_of::<Block>();
        const MAX_BLOCKS_LEN: usize = MAX_HASHED_BUFFER_LEN / std::mem::size_of::<Block>();

        // Partially hash the `blocks` buffer when it is large. To limit collisions with common
        // prefixes and suffixes, we hash the length and some slices of the buffer.
        let block_count = self.blocks.len();
        if block_count > MAX_BLOCKS_LEN {
            // Hash the buffer's length.
            block_count.hash(state);

            // And its head and tail.
            self.blocks[..MAX_BLOCKS_TO_HASH].hash(state);
            self.blocks[block_count - MAX_BLOCKS_TO_HASH..].hash(state);
        } else {
            self.blocks.hash(state);
        }

        // Hash the other fields as usual.
        self.len.hash(state);
    }
}

impl InitMask {
    pub const BLOCK_SIZE: u64 = 64;

    #[inline]
    fn bit_index(bits: Size) -> (usize, usize) {
        // BLOCK_SIZE is the number of bits that can fit in a `Block`.
        // Each bit in a `Block` represents the initialization state of one byte of an allocation,
        // so we use `.bytes()` here.
        let bits = bits.bytes();
        let a = bits / InitMask::BLOCK_SIZE;
        let b = bits % InitMask::BLOCK_SIZE;
        (usize::try_from(a).unwrap(), usize::try_from(b).unwrap())
    }

    #[inline]
    fn size_from_bit_index(block: impl TryInto<u64>, bit: impl TryInto<u64>) -> Size {
        let block = block.try_into().ok().unwrap();
        let bit = bit.try_into().ok().unwrap();
        Size::from_bytes(block * InitMask::BLOCK_SIZE + bit)
    }

    pub fn new(size: Size, state: bool) -> Self {
        let mut m = InitMask { blocks: vec![], len: Size::ZERO };
        m.grow(size, state);
        m
    }

    pub fn set_range(&mut self, start: Size, end: Size, new_state: bool) {
        let len = self.len;
        if end > len {
            self.grow(end - len, new_state);
        }
        self.set_range_inbounds(start, end, new_state);
    }

    pub fn set_range_inbounds(&mut self, start: Size, end: Size, new_state: bool) {
        let (blocka, bita) = Self::bit_index(start);
        let (blockb, bitb) = Self::bit_index(end);
        if blocka == blockb {
            // First set all bits except the first `bita`,
            // then unset the last `64 - bitb` bits.
            let range = if bitb == 0 {
                u64::MAX << bita
            } else {
                (u64::MAX << bita) & (u64::MAX >> (64 - bitb))
            };
            if new_state {
                self.blocks[blocka] |= range;
            } else {
                self.blocks[blocka] &= !range;
            }
            return;
        }
        // across block boundaries
        if new_state {
            // Set `bita..64` to `1`.
            self.blocks[blocka] |= u64::MAX << bita;
            // Set `0..bitb` to `1`.
            if bitb != 0 {
                self.blocks[blockb] |= u64::MAX >> (64 - bitb);
            }
            // Fill in all the other blocks (much faster than one bit at a time).
            for block in (blocka + 1)..blockb {
                self.blocks[block] = u64::MAX;
            }
        } else {
            // Set `bita..64` to `0`.
            self.blocks[blocka] &= !(u64::MAX << bita);
            // Set `0..bitb` to `0`.
            if bitb != 0 {
                self.blocks[blockb] &= !(u64::MAX >> (64 - bitb));
            }
            // Fill in all the other blocks (much faster than one bit at a time).
            for block in (blocka + 1)..blockb {
                self.blocks[block] = 0;
            }
        }
    }

    #[inline]
    pub fn get(&self, i: Size) -> bool {
        let (block, bit) = Self::bit_index(i);
        (self.blocks[block] & (1 << bit)) != 0
    }

    #[inline]
    pub fn set(&mut self, i: Size, new_state: bool) {
        let (block, bit) = Self::bit_index(i);
        self.set_bit(block, bit, new_state);
    }

    #[inline]
    fn set_bit(&mut self, block: usize, bit: usize, new_state: bool) {
        if new_state {
            self.blocks[block] |= 1 << bit;
        } else {
            self.blocks[block] &= !(1 << bit);
        }
    }

    pub fn grow(&mut self, amount: Size, new_state: bool) {
        if amount.bytes() == 0 {
            return;
        }
        let unused_trailing_bits =
            u64::try_from(self.blocks.len()).unwrap() * Self::BLOCK_SIZE - self.len.bytes();
        if amount.bytes() > unused_trailing_bits {
            let additional_blocks = amount.bytes() / Self::BLOCK_SIZE + 1;
            self.blocks.extend(
                // FIXME(oli-obk): optimize this by repeating `new_state as Block`.
                iter::repeat(0).take(usize::try_from(additional_blocks).unwrap()),
            );
        }
        let start = self.len;
        self.len += amount;
        self.set_range_inbounds(start, start + amount, new_state); // `Size` operation
    }

    /// Returns the index of the first bit in `start..end` (end-exclusive) that is equal to is_init.
    fn find_bit(&self, start: Size, end: Size, is_init: bool) -> Option<Size> {
        /// A fast implementation of `find_bit`,
        /// which skips over an entire block at a time if it's all 0s (resp. 1s),
        /// and finds the first 1 (resp. 0) bit inside a block using `trailing_zeros` instead of a loop.
        ///
        /// Note that all examples below are written with 8 (instead of 64) bit blocks for simplicity,
        /// and with the least significant bit (and lowest block) first:
        /// ```text
        ///        00000000|00000000
        ///        ^      ^ ^      ^
        /// index: 0      7 8      15
        /// ```
        /// Also, if not stated, assume that `is_init = true`, that is, we are searching for the first 1 bit.
        fn find_bit_fast(
            init_mask: &InitMask,
            start: Size,
            end: Size,
            is_init: bool,
        ) -> Option<Size> {
            /// Search one block, returning the index of the first bit equal to `is_init`.
            fn search_block(
                bits: Block,
                block: usize,
                start_bit: usize,
                is_init: bool,
            ) -> Option<Size> {
                // For the following examples, assume this function was called with:
                //   bits = 0b00111011
                //   start_bit = 3
                //   is_init = false
                // Note that, for the examples in this function, the most significant bit is written first,
                // which is backwards compared to the comments in `find_bit`/`find_bit_fast`.

                // Invert bits so we're always looking for the first set bit.
                //        ! 0b00111011
                //   bits = 0b11000100
                let bits = if is_init { bits } else { !bits };
                // Mask off unused start bits.
                //          0b11000100
                //        & 0b11111000
                //   bits = 0b11000000
                let bits = bits & (!0 << start_bit);
                // Find set bit, if any.
                //   bit = trailing_zeros(0b11000000)
                //   bit = 6
                if bits == 0 {
                    None
                } else {
                    let bit = bits.trailing_zeros();
                    Some(InitMask::size_from_bit_index(block, bit))
                }
            }

            if start >= end {
                return None;
            }

            // Convert `start` and `end` to block indexes and bit indexes within each block.
            // We must convert `end` to an inclusive bound to handle block boundaries correctly.
            //
            // For example:
            //
            //   (a) 00000000|00000000    (b) 00000000|
            //       ^~~~~~~~~~~^             ^~~~~~~~~^
            //     start       end          start     end
            //
            // In both cases, the block index of `end` is 1.
            // But we do want to search block 1 in (a), and we don't in (b).
            //
            // We subtract 1 from both end positions to make them inclusive:
            //
            //   (a) 00000000|00000000    (b) 00000000|
            //       ^~~~~~~~~~^              ^~~~~~~^
            //     start    end_inclusive   start end_inclusive
            //
            // For (a), the block index of `end_inclusive` is 1, and for (b), it's 0.
            // This provides the desired behavior of searching blocks 0 and 1 for (a),
            // and searching only block 0 for (b).
            // There is no concern of overflows since we checked for `start >= end` above.
            let (start_block, start_bit) = InitMask::bit_index(start);
            let end_inclusive = Size::from_bytes(end.bytes() - 1);
            let (end_block_inclusive, _) = InitMask::bit_index(end_inclusive);

            // Handle first block: need to skip `start_bit` bits.
            //
            // We need to handle the first block separately,
            // because there may be bits earlier in the block that should be ignored,
            // such as the bit marked (1) in this example:
            //
            //       (1)
            //       -|------
            //   (c) 01000000|00000000|00000001
            //          ^~~~~~~~~~~~~~~~~~^
            //        start              end
            if let Some(i) =
                search_block(init_mask.blocks[start_block], start_block, start_bit, is_init)
            {
                // If the range is less than a block, we may find a matching bit after `end`.
                //
                // For example, we shouldn't successfully find bit (2), because it's after `end`:
                //
                //             (2)
                //       -------|
                //   (d) 00000001|00000000|00000001
                //        ^~~~~^
                //      start end
                //
                // An alternative would be to mask off end bits in the same way as we do for start bits,
                // but performing this check afterwards is faster and simpler to implement.
                if i < end {
                    return Some(i);
                } else {
                    return None;
                }
            }

            // Handle remaining blocks.
            //
            // We can skip over an entire block at once if it's all 0s (resp. 1s).
            // The block marked (3) in this example is the first block that will be handled by this loop,
            // and it will be skipped for that reason:
            //
            //                   (3)
            //                --------
            //   (e) 01000000|00000000|00000001
            //          ^~~~~~~~~~~~~~~~~~^
            //        start              end
            if start_block < end_block_inclusive {
                // This loop is written in a specific way for performance.
                // Notably: `..end_block_inclusive + 1` is used for an inclusive range instead of `..=end_block_inclusive`,
                // and `.zip(start_block + 1..)` is used to track the index instead of `.enumerate().skip().take()`,
                // because both alternatives result in significantly worse codegen.
                // `end_block_inclusive + 1` is guaranteed not to wrap, because `end_block_inclusive <= end / BLOCK_SIZE`,
                // and `BLOCK_SIZE` (the number of bits per block) will always be at least 8 (1 byte).
                for (&bits, block) in init_mask.blocks[start_block + 1..end_block_inclusive + 1]
                    .iter()
                    .zip(start_block + 1..)
                {
                    if let Some(i) = search_block(bits, block, 0, is_init) {
                        // If this is the last block, we may find a matching bit after `end`.
                        //
                        // For example, we shouldn't successfully find bit (4), because it's after `end`:
                        //
                        //                               (4)
                        //                         -------|
                        //   (f) 00000001|00000000|00000001
                        //          ^~~~~~~~~~~~~~~~~~^
                        //        start              end
                        //
                        // As above with example (d), we could handle the end block separately and mask off end bits,
                        // but unconditionally searching an entire block at once and performing this check afterwards
                        // is faster and much simpler to implement.
                        if i < end {
                            return Some(i);
                        } else {
                            return None;
                        }
                    }
                }
            }

            None
        }

        #[cfg_attr(not(debug_assertions), allow(dead_code))]
        fn find_bit_slow(
            init_mask: &InitMask,
            start: Size,
            end: Size,
            is_init: bool,
        ) -> Option<Size> {
            (start..end).find(|&i| init_mask.get(i) == is_init)
        }

        let result = find_bit_fast(self, start, end, is_init);

        debug_assert_eq!(
            result,
            find_bit_slow(self, start, end, is_init),
            "optimized implementation of find_bit is wrong for start={:?} end={:?} is_init={} init_mask={:#?}",
            start,
            end,
            is_init,
            self
        );

        result
    }
}

/// A contiguous chunk of initialized or uninitialized memory.
pub enum InitChunk {
    Init(Range<Size>),
    Uninit(Range<Size>),
}

impl InitChunk {
    #[inline]
    pub fn is_init(&self) -> bool {
        match self {
            Self::Init(_) => true,
            Self::Uninit(_) => false,
        }
    }

    #[inline]
    pub fn range(&self) -> Range<Size> {
        match self {
            Self::Init(r) => r.clone(),
            Self::Uninit(r) => r.clone(),
        }
    }
}

impl InitMask {
    /// Checks whether the range `start..end` (end-exclusive) is entirely initialized.
    ///
    /// Returns `Ok(())` if it's initialized. Otherwise returns a range of byte
    /// indexes for the first contiguous span of the uninitialized access.
    #[inline]
    pub fn is_range_initialized(&self, start: Size, end: Size) -> Result<(), Range<Size>> {
        if end > self.len {
            return Err(self.len..end);
        }

        let uninit_start = self.find_bit(start, end, false);

        match uninit_start {
            Some(uninit_start) => {
                let uninit_end = self.find_bit(uninit_start, end, true).unwrap_or(end);
                Err(uninit_start..uninit_end)
            }
            None => Ok(()),
        }
    }

    /// Returns an iterator, yielding a range of byte indexes for each contiguous region
    /// of initialized or uninitialized bytes inside the range `start..end` (end-exclusive).
    ///
    /// The iterator guarantees the following:
    /// - Chunks are nonempty.
    /// - Chunks are adjacent (each range's start is equal to the previous range's end).
    /// - Chunks span exactly `start..end` (the first starts at `start`, the last ends at `end`).
    /// - Chunks alternate between [`InitChunk::Init`] and [`InitChunk::Uninit`].
    #[inline]
    pub fn range_as_init_chunks(&self, start: Size, end: Size) -> InitChunkIter<'_> {
        assert!(end <= self.len);

        let is_init = if start < end {
            self.get(start)
        } else {
            // `start..end` is empty: there are no chunks, so use some arbitrary value
            false
        };

        InitChunkIter { init_mask: self, is_init, start, end }
    }
}

/// Yields [`InitChunk`]s. See [`InitMask::range_as_init_chunks`].
#[derive(Clone)]
pub struct InitChunkIter<'a> {
    init_mask: &'a InitMask,
    /// Whether the next chunk we will return is initialized.
    /// If there are no more chunks, contains some arbitrary value.
    is_init: bool,
    /// The current byte index into `init_mask`.
    start: Size,
    /// The end byte index into `init_mask`.
    end: Size,
}

impl<'a> Iterator for InitChunkIter<'a> {
    type Item = InitChunk;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.start >= self.end {
            return None;
        }

        let end_of_chunk =
            self.init_mask.find_bit(self.start, self.end, !self.is_init).unwrap_or(self.end);
        let range = self.start..end_of_chunk;

        let ret =
            Some(if self.is_init { InitChunk::Init(range) } else { InitChunk::Uninit(range) });

        self.is_init = !self.is_init;
        self.start = end_of_chunk;

        ret
    }
}

/// Uninitialized bytes.
impl<Tag: Copy, Extra> Allocation<Tag, Extra> {
    /// Checks whether the given range  is entirely initialized.
    ///
    /// Returns `Ok(())` if it's initialized. Otherwise returns the range of byte
    /// indexes of the first contiguous uninitialized access.
    fn is_init(&self, range: AllocRange) -> Result<(), Range<Size>> {
        self.init_mask.is_range_initialized(range.start, range.end()) // `Size` addition
    }

    /// Checks that a range of bytes is initialized. If not, returns the `InvalidUninitBytes`
    /// error which will report the first range of bytes which is uninitialized.
    fn check_init(&self, range: AllocRange) -> AllocResult {
        self.is_init(range).map_err(|idx_range| {
            AllocError::InvalidUninitBytes(Some(UninitBytesAccess {
                access_offset: range.start,
                access_size: range.size,
                uninit_offset: idx_range.start,
                uninit_size: idx_range.end - idx_range.start, // `Size` subtraction
            }))
        })
    }

    fn mark_init(&mut self, range: AllocRange, is_init: bool) {
        if range.size.bytes() == 0 {
            return;
        }
        assert!(self.mutability == Mutability::Mut);
        self.init_mask.set_range(range.start, range.end(), is_init);
    }
}

/// Run-length encoding of the uninit mask.
/// Used to copy parts of a mask multiple times to another allocation.
pub struct InitMaskCompressed {
    /// Whether the first range is initialized.
    initial: bool,
    /// The lengths of ranges that are run-length encoded.
    /// The initialization state of the ranges alternate starting with `initial`.
    ranges: smallvec::SmallVec<[u64; 1]>,
}

impl InitMaskCompressed {
    pub fn no_bytes_init(&self) -> bool {
        // The `ranges` are run-length encoded and of alternating initialization state.
        // So if `ranges.len() > 1` then the second block is an initialized range.
        !self.initial && self.ranges.len() == 1
    }
}

/// Transferring the initialization mask to other allocations.
impl<Tag, Extra> Allocation<Tag, Extra> {
    /// Creates a run-length encoding of the initialization mask; panics if range is empty.
    ///
    /// This is essentially a more space-efficient version of
    /// `InitMask::range_as_init_chunks(...).collect::<Vec<_>>()`.
    pub fn compress_uninit_range(&self, range: AllocRange) -> InitMaskCompressed {
        // Since we are copying `size` bytes from `src` to `dest + i * size` (`for i in 0..repeat`),
        // a naive initialization mask copying algorithm would repeatedly have to read the initialization mask from
        // the source and write it to the destination. Even if we optimized the memory accesses,
        // we'd be doing all of this `repeat` times.
        // Therefore we precompute a compressed version of the initialization mask of the source value and
        // then write it back `repeat` times without computing any more information from the source.

        // A precomputed cache for ranges of initialized / uninitialized bits
        // 0000010010001110 will become
        // `[5, 1, 2, 1, 3, 3, 1]`,
        // where each element toggles the state.

        let mut ranges = smallvec::SmallVec::<[u64; 1]>::new();

        let mut chunks = self.init_mask.range_as_init_chunks(range.start, range.end()).peekable();

        let initial = chunks.peek().expect("range should be nonempty").is_init();

        // Here we rely on `range_as_init_chunks` to yield alternating init/uninit chunks.
        for chunk in chunks {
            let len = chunk.range().end.bytes() - chunk.range().start.bytes();
            ranges.push(len);
        }

        InitMaskCompressed { ranges, initial }
    }

    /// Applies multiple instances of the run-length encoding to the initialization mask.
    ///
    /// This is dangerous to use as it can violate internal `Allocation` invariants!
    /// It only exists to support an efficient implementation of `mem_copy_repeatedly`.
    pub fn mark_compressed_init_range(
        &mut self,
        defined: &InitMaskCompressed,
        range: AllocRange,
        repeat: u64,
    ) {
        // An optimization where we can just overwrite an entire range of initialization
        // bits if they are going to be uniformly `1` or `0`.
        if defined.ranges.len() <= 1 {
            self.init_mask.set_range_inbounds(
                range.start,
                range.start + range.size * repeat, // `Size` operations
                defined.initial,
            );
            return;
        }

        for mut j in 0..repeat {
            j *= range.size.bytes();
            j += range.start.bytes();
            let mut cur = defined.initial;
            for range in &defined.ranges {
                let old_j = j;
                j += range;
                self.init_mask.set_range_inbounds(
                    Size::from_bytes(old_j),
                    Size::from_bytes(j),
                    cur,
                );
                cur = !cur;
            }
        }
    }
}
